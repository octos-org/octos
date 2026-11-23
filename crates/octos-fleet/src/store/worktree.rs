//! The durable records behind a task that runs in a git worktree.
//!
//! A worktree task is never restarted from its branch's last commit when an
//! attempt is interrupted: it recovers what the attempt left. For that the
//! kernel remembers three things:
//!
//! - **the base** — [`FleetKernelStore::bind_worktree`] records the worktree
//!   and the immutable base revision once per fleet generation, before the
//!   worker edits anything;
//! - **which directory holds the work** —
//!   [`FleetKernelStore::mark_worktree_populated`]. Every attempt works in a
//!   directory of its own; the record moves to it only when it holds the tree;
//! - **every acceptance run** — [`FleetKernelStore::begin_verification`] writes the
//!   intent (command + exact tree) before the command starts and
//!   [`FleetKernelStore::commit_verification`] writes its outcome.
//!   [`FleetKernelStore::find_verification`] reads back a committed outcome for
//!   an exact run, so a caller can decide not to repeat it; an intent with no
//!   outcome is an unknown result, never a verdict.
//!
//! Every write here is fenced by the SAME four-part predicate as
//! [`FleetKernelStore::complete_child`] — the child's current attempt, attempt
//! `Running`, attempt generation == fleet generation, lease `owner_epoch` — and
//! by the terminal-fleet fence. A stale attempt's write is a typed `Superseded`
//! with zero mutation, never an error.

use super::*;

/// Result of [`FleetKernelStore::bind_worktree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindWorktreeOutcome {
    /// The binding now in force for this generation. Its `base_commit` /
    /// `base_tree` are the ones recorded when the generation was FIRST bound,
    /// whatever the request proposed.
    Bound(WorktreeBinding),
    /// The attempt lost the fence; nothing changed.
    Superseded,
}

/// Result of [`FleetKernelStore::begin_verification`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginVerification {
    /// The intent is recorded: the caller runs the command, then commits its
    /// outcome under `op_id`.
    Run {
        op_id: String,
        /// How many earlier intents for this exact run have no outcome — runs
        /// that were started and whose result is unknown.
        unknown_prior: usize,
    },
    /// The attempt lost the fence; nothing changed.
    Superseded,
}

/// The rows a fenced worktree write acts on.
struct Fenced {
    child: FleetChildRecord,
    child_key: String,
    fleet_generation: u64,
}

/// Load the child, attempt and fleet rows under the caller's write txn and
/// apply `complete_child`'s identity checks, terminal-fleet fence and four-part
/// predicate. `Ok(None)` is a lost fence (the caller returns its `Superseded`).
fn load_fenced(
    children: &impl ReadableTable<&'static str, &'static str>,
    attempts: &impl ReadableTable<&'static str, &'static str>,
    fleets: &impl ReadableTable<&'static str, &'static str>,
    fleet_id: &str,
    child_id: &str,
    attempt_id: &str,
    owner_epoch: u64,
) -> Result<Option<Fenced>> {
    let ckey = child_key(fleet_id, child_id);
    let akey = attempt_key(child_id, attempt_id);

    let Some(cj) = children.get(ckey.as_str())?.map(|g| g.value().to_string()) else {
        return Ok(None);
    };
    let Some(child) = decode_row::<FleetChildRecord>(&cj)? else {
        return Ok(None);
    };
    let Some(aj) = attempts.get(akey.as_str())?.map(|g| g.value().to_string()) else {
        return Ok(None);
    };
    let Some(attempt) = decode_row::<Attempt>(&aj)? else {
        return Ok(None);
    };
    let Some(fj) = fleets.get(fleet_id)?.map(|g| g.value().to_string()) else {
        return Ok(None);
    };
    let Some(fleet) = decode_row::<FleetRecord>(&fj)? else {
        return Ok(None);
    };

    if child.fleet_id != fleet_id
        || child.child_id != child_id
        || attempt.fleet_id != fleet_id
        || attempt.child_id != child_id
        || attempt.attempt_id != attempt_id
        || fleet.fleet_id != fleet_id
    {
        return Ok(None);
    }
    if !fleet_is_live(&fleet) {
        return Ok(None);
    }
    let is_current = child.current_attempt_id.as_deref() == Some(attempt_id);
    let is_running = attempt.status == AttemptStatus::Running;
    let gen_ok = attempt.generation == fleet.generation;
    let lease_ok = attempt.lease.owner_epoch == owner_epoch;
    if !(is_current && is_running && gen_ok && lease_ok) {
        return Ok(None);
    }
    Ok(Some(Fenced {
        child,
        child_key: ckey,
        fleet_generation: fleet.generation,
    }))
}

impl FleetKernelStore {
    /// Bind a task to its worktree and base revision.
    ///
    /// The first bind of a fleet generation records the request: its
    /// `base_commit` / `base_tree`, and its (still unpopulated) checkout. Every
    /// later bind of the SAME generation changes NOTHING and hands the stored
    /// binding back — the base is immutable however far the branch has advanced
    /// and whatever the caller read this time, and the recorded checkout keeps
    /// naming the directory that holds the work until
    /// [`Self::mark_worktree_populated`] moves it. A bind under a NEW generation
    /// (after a replan) replaces the binding.
    #[allow(clippy::too_many_arguments)] // 4-part predicate + the request are irreducible here
    pub async fn bind_worktree(
        &self,
        fleet_id: &str,
        child_id: &str,
        attempt_id: &str,
        owner_epoch: u64,
        request: WorktreeBindRequest,
        now_ms: u64,
    ) -> Result<BindWorktreeOutcome> {
        ensure_key_safe(&[fleet_id, child_id, attempt_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let attempt_id = attempt_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<BindWorktreeOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            let binding = {
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let attempts = wtx.open_table(ATTEMPTS)?;
                let fleets = wtx.open_table(FLEETS)?;
                let Some(mut fenced) = load_fenced(
                    &children,
                    &attempts,
                    &fleets,
                    &fleet_id,
                    &child_id,
                    &attempt_id,
                    owner_epoch,
                )?
                else {
                    return Ok(BindWorktreeOutcome::Superseded);
                };

                if let Some(existing) = fenced.child.worktree.as_ref() {
                    if existing.generation == fenced.fleet_generation {
                        return Ok(BindWorktreeOutcome::Bound(existing.clone()));
                    }
                }
                let binding = WorktreeBinding {
                    generation: fenced.fleet_generation,
                    repo_root: request.repo_root,
                    branch: request.branch,
                    checkout: request.checkout,
                    base_commit: request.base_commit,
                    base_tree: request.base_tree,
                    bound_at_ms: now_ms,
                    populated: false,
                };
                fenced.child.worktree = Some(binding.clone());
                fenced.child.updated_at_ms = now_ms;
                children.insert(
                    fenced.child_key.as_str(),
                    serde_json::to_string(&fenced.child)?.as_str(),
                )?;
                binding
            };
            wtx.commit()?;
            Ok(BindWorktreeOutcome::Bound(binding))
        })
        .await
        .wrap_err("join bind_worktree")?
    }

    /// Record that `checkout` now holds the task's tree: the worker finished
    /// populating it, so a later attempt may trust and recover what it finds
    /// there. The binding's checkout moves to `checkout` and becomes populated
    /// in ONE write — the record never names a directory that is still empty as
    /// the one holding the work. A missing binding (or one from an older
    /// generation) is an `Err`: populate is only ever recorded after a
    /// successful [`Self::bind_worktree`].
    pub async fn mark_worktree_populated(
        &self,
        fleet_id: &str,
        child_id: &str,
        attempt_id: &str,
        owner_epoch: u64,
        checkout: &str,
        now_ms: u64,
    ) -> Result<CompleteOutcome> {
        ensure_key_safe(&[fleet_id, child_id, attempt_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let attempt_id = attempt_id.to_string();
        let checkout = checkout.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<CompleteOutcome> {
            let _held = held;
            let wtx = db.begin_write()?;
            {
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let attempts = wtx.open_table(ATTEMPTS)?;
                let fleets = wtx.open_table(FLEETS)?;
                let Some(mut fenced) = load_fenced(
                    &children,
                    &attempts,
                    &fleets,
                    &fleet_id,
                    &child_id,
                    &attempt_id,
                    owner_epoch,
                )?
                else {
                    return Ok(CompleteOutcome::Superseded);
                };
                let generation = fenced.fleet_generation;
                match fenced.child.worktree.as_mut() {
                    Some(binding) if binding.generation == generation => {
                        binding.checkout = checkout;
                        binding.populated = true;
                    }
                    _ => bail!(
                        "mark_worktree_populated: {fleet_id}/{child_id} has no worktree bound \
                         for generation {generation}"
                    ),
                }
                fenced.child.updated_at_ms = now_ms;
                children.insert(
                    fenced.child_key.as_str(),
                    serde_json::to_string(&fenced.child)?.as_str(),
                )?;
            }
            wtx.commit()?;
            Ok(CompleteOutcome::Completed)
        })
        .await
        .wrap_err("join mark_worktree_populated")?
    }

    /// The outcome already committed for an exact run, if there is one.
    ///
    /// "The exact run" is the current fleet generation plus the request's
    /// phase, criterion, command, commit and tree digest. Only a record that
    /// carries an exit code counts: an intent with no outcome, or an outcome
    /// with no exit code (the process was ended by a signal), is not a verdict.
    /// When several qualify the latest is returned.
    ///
    /// A plain read, not fenced: a superseded attempt may learn what was
    /// committed, and every write it would base on that is refused.
    pub async fn find_verification(
        &self,
        fleet_id: &str,
        child_id: &str,
        request: &VerificationRequest,
    ) -> Result<Option<VerificationRecord>> {
        ensure_key_safe(&[fleet_id, child_id])?;
        let Some(fleet) = self.get_fleet(fleet_id).await? else {
            return Ok(None);
        };
        let Some(child) = self.get_child(fleet_id, child_id).await? else {
            return Ok(None);
        };
        Ok(child.verifications.into_iter().rev().find(|record| {
            record.is_run(fleet.generation, request)
                && record
                    .result
                    .as_ref()
                    .is_some_and(|result| result.exit_status.is_some())
        }))
    }

    /// Record the INTENT to run `request.command` against the exact tree
    /// `(request.commit, request.tree_digest)`, before the command starts.
    ///
    /// A new intent is always appended — whether to run at all is the caller's
    /// decision, taken from [`Self::find_verification`]. Earlier intents for
    /// the same exact run that have no outcome are counted in `unknown_prior`.
    #[allow(clippy::too_many_arguments)] // 4-part predicate + the request are irreducible here
    pub async fn begin_verification(
        &self,
        fleet_id: &str,
        child_id: &str,
        attempt_id: &str,
        owner_epoch: u64,
        request: VerificationRequest,
        now_ms: u64,
    ) -> Result<BeginVerification> {
        ensure_key_safe(&[fleet_id, child_id, attempt_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let attempt_id = attempt_id.to_string();
        let op_id = Uuid::new_v4().to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<BeginVerification> {
            let _held = held;
            let wtx = db.begin_write()?;
            let unknown_prior = {
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let attempts = wtx.open_table(ATTEMPTS)?;
                let fleets = wtx.open_table(FLEETS)?;
                let Some(mut fenced) = load_fenced(
                    &children,
                    &attempts,
                    &fleets,
                    &fleet_id,
                    &child_id,
                    &attempt_id,
                    owner_epoch,
                )?
                else {
                    return Ok(BeginVerification::Superseded);
                };
                let generation = fenced.fleet_generation;
                let unknown_prior = fenced
                    .child
                    .verifications
                    .iter()
                    .filter(|r| r.is_run(generation, &request) && r.result.is_none())
                    .count();
                fenced.child.verifications.push(VerificationRecord {
                    op_id: op_id.clone(),
                    attempt_id: attempt_id.clone(),
                    generation,
                    phase: request.phase,
                    criterion_id: request.criterion_id,
                    command: request.command,
                    commit: request.commit,
                    tree_digest: request.tree_digest,
                    started_at_ms: now_ms,
                    result: None,
                });
                fenced.child.updated_at_ms = now_ms;
                children.insert(
                    fenced.child_key.as_str(),
                    serde_json::to_string(&fenced.child)?.as_str(),
                )?;
                unknown_prior
            };
            wtx.commit()?;
            Ok(BeginVerification::Run {
                op_id,
                unknown_prior,
            })
        })
        .await
        .wrap_err("join begin_verification")?
    }

    /// Commit the outcome of the run recorded as `op_id`, returning the
    /// completed record. `Ok(None)` when the attempt lost the fence or `op_id`
    /// is not one of ITS OWN intents — a late result from a superseded attempt
    /// is dropped, never attached. Committing an already-committed run returns
    /// the stored record unchanged.
    #[allow(clippy::too_many_arguments)] // 4-part predicate + the outcome are irreducible here
    pub async fn commit_verification(
        &self,
        fleet_id: &str,
        child_id: &str,
        attempt_id: &str,
        owner_epoch: u64,
        op_id: &str,
        exit_status: Option<i32>,
        output_sha256: String,
        now_ms: u64,
    ) -> Result<Option<VerificationRecord>> {
        ensure_key_safe(&[fleet_id, child_id, attempt_id])?;
        let db = self.db.clone();
        let fleet_id = fleet_id.to_string();
        let child_id = child_id.to_string();
        let attempt_id = attempt_id.to_string();
        let op_id = op_id.to_string();
        let held = self.io_gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<Option<VerificationRecord>> {
            let _held = held;
            let wtx = db.begin_write()?;
            let record = {
                let mut children = wtx.open_table(FLEET_CHILDREN)?;
                let attempts = wtx.open_table(ATTEMPTS)?;
                let fleets = wtx.open_table(FLEETS)?;
                let Some(mut fenced) = load_fenced(
                    &children,
                    &attempts,
                    &fleets,
                    &fleet_id,
                    &child_id,
                    &attempt_id,
                    owner_epoch,
                )?
                else {
                    return Ok(None);
                };
                let Some(record) = fenced
                    .child
                    .verifications
                    .iter_mut()
                    .find(|r| r.op_id == op_id && r.attempt_id == attempt_id)
                else {
                    return Ok(None);
                };
                if record.result.is_some() {
                    return Ok(Some(record.clone()));
                }
                record.result = Some(VerificationResult {
                    exit_status,
                    output_sha256,
                    finished_at_ms: now_ms,
                });
                let record = record.clone();
                fenced.child.updated_at_ms = now_ms;
                children.insert(
                    fenced.child_key.as_str(),
                    serde_json::to_string(&fenced.child)?.as_str(),
                )?;
                record
            };
            wtx.commit()?;
            Ok(Some(record))
        })
        .await
        .wrap_err("join commit_verification")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const EPOCH: u64 = 100;
    const TTL: u64 = 1_000;

    async fn fresh() -> (TempDir, FleetKernelStore) {
        let dir = TempDir::new().unwrap();
        let store = FleetKernelStore::open(dir.path()).await.unwrap();
        (dir, store)
    }

    /// A fleet `f1` with one dep-free (immediately `Ready`) child `c1`.
    async fn fleet_with_ready_child(store: &FleetKernelStore) {
        store
            .create_fleet(
                "f1",
                SessionKey::new("fleet", "controller-1"),
                None,
                "default",
                10_000,
                false,
                0,
            )
            .await
            .unwrap();
        store.add_child("f1", "c1", vec![], 0).await.unwrap();
    }

    /// Launch + mark_running `c1` under `epoch`, returning the attempt id.
    async fn launch_running(store: &FleetKernelStore, epoch: u64) -> String {
        let id = match store
            .launch_child("f1", "c1", 100, 0, epoch, TTL)
            .await
            .unwrap()
        {
            LaunchOutcome::Launched { attempt_id } => attempt_id,
            other => panic!("{other:?}"),
        };
        store.mark_running("c1", &id).await.unwrap();
        id
    }

    /// The host dies and boots again as `new_epoch`: the live attempt is
    /// interrupted and the child returns to `Ready`.
    async fn reboot(store: &FleetKernelStore, new_epoch: u64) {
        let report = store.reconcile(5_000, new_epoch).await.unwrap();
        assert_eq!(
            report.interrupted.len(),
            1,
            "the dead attempt is interrupted"
        );
    }

    fn bind_request(base_commit: &str, base_tree: &str, checkout: &str) -> WorktreeBindRequest {
        WorktreeBindRequest {
            repo_root: "/repo".into(),
            branch: "fleet/f1/c1".into(),
            checkout: checkout.into(),
            base_commit: base_commit.into(),
            base_tree: base_tree.into(),
        }
    }

    fn run_request(commit: &str, tree: &str) -> VerificationRequest {
        VerificationRequest {
            phase: VerificationPhase::Acceptance,
            criterion_id: "test".into(),
            command: "cargo test named".into(),
            commit: commit.into(),
            tree_digest: tree.into(),
        }
    }

    async fn bound(
        store: &FleetKernelStore,
        attempt: &str,
        epoch: u64,
        request: WorktreeBindRequest,
    ) -> WorktreeBinding {
        match store
            .bind_worktree("f1", "c1", attempt, epoch, request, 10)
            .await
            .unwrap()
        {
            BindWorktreeOutcome::Bound(binding) => binding,
            other => panic!("expected Bound, got {other:?}"),
        }
    }

    async fn verifications(store: &FleetKernelStore) -> Vec<VerificationRecord> {
        store
            .get_child("f1", "c1")
            .await
            .unwrap()
            .unwrap()
            .verifications
    }

    #[tokio::test]
    async fn should_keep_base_immutable_when_later_attempts_bind_again() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store).await;

        // Attempt 1 binds the generation: the base is recorded, and its fresh
        // checkout is not populated yet.
        let a1 = launch_running(&store, EPOCH).await;
        let first = bound(&store, &a1, EPOCH, bind_request("c0", "t0", "/work/1")).await;
        assert_eq!(first.base_commit, "c0");
        assert_eq!(first.base_tree, "t0");
        assert_eq!(first.generation, 0);
        assert_eq!(first.checkout, "/work/1");
        assert!(!first.populated);
        assert_eq!(
            store
                .mark_worktree_populated("f1", "c1", &a1, EPOCH, "/work/1", 11)
                .await
                .unwrap(),
            CompleteOutcome::Completed,
        );

        // The host dies. Attempt 2 runs in a directory of its own; the branch
        // has advanced to c5. The binding it reads back is unchanged: the base
        // is still c0, and the work is still recorded where attempt 1 left it.
        reboot(&store, EPOCH + 1).await;
        let a2 = launch_running(&store, EPOCH + 1).await;
        let second = bound(&store, &a2, EPOCH + 1, bind_request("c5", "t5", "/work/2")).await;
        assert_eq!(second.base_commit, "c0", "the base is immutable");
        assert_eq!(second.base_tree, "t0", "the base is immutable");
        assert_eq!(second.bound_at_ms, first.bound_at_ms);
        assert_eq!(
            (second.checkout.as_str(), second.populated),
            ("/work/1", true),
            "binding again does not move the record to a directory that is still empty",
        );

        // Attempt 2 dies before its directory holds anything: the record still
        // names attempt 1's. Attempt 3 finishes populating its own, and only
        // then does the record move — directory and flag together.
        reboot(&store, EPOCH + 2).await;
        let a3 = launch_running(&store, EPOCH + 2).await;
        let third = bound(&store, &a3, EPOCH + 2, bind_request("c5", "t5", "/work/3")).await;
        assert_eq!(third, second);
        store
            .mark_worktree_populated("f1", "c1", &a3, EPOCH + 2, "/work/3", 50)
            .await
            .unwrap();

        let stored = store.get_child("f1", "c1").await.unwrap().unwrap();
        assert_eq!(
            stored.worktree,
            Some(WorktreeBinding {
                checkout: "/work/3".into(),
                populated: true,
                ..first
            }),
        );
    }

    #[tokio::test]
    async fn should_bind_a_new_base_when_fleet_generation_changes() {
        let (_d, store) = fresh().await;
        store
            .create_fleet_with_plan(
                SessionKey::new("fleet", "controller-1"),
                None,
                "default",
                10_000,
                false,
                DurablePlan {
                    schema_version: SCHEMA_VERSION,
                    fleet_id: "f1".into(),
                    revision: 0,
                    objective: "obj".into(),
                    tasks: vec![plan_task("c1", "first detail")],
                },
                0,
            )
            .await
            .unwrap();
        let a1 = launch_running(&store, EPOCH).await;
        let first = bound(&store, &a1, EPOCH, bind_request("c0", "t0", "/work/1")).await;
        assert_eq!(first.generation, 0);

        // A replan bumps the fleet generation and interrupts the live attempt.
        let outcome = store
            .replan(
                "f1",
                0,
                DurablePlan {
                    schema_version: SCHEMA_VERSION,
                    fleet_id: "f1".into(),
                    revision: 0,
                    objective: "obj".into(),
                    tasks: vec![plan_task("c1", "changed detail")],
                },
                20,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, PlanMutateOutcome::Mutated { .. }));
        let generation = store.get_fleet("f1").await.unwrap().unwrap().generation;
        assert!(generation > 0, "the replan bumped the generation");

        let a2 = launch_running(&store, EPOCH).await;
        let second = bound(&store, &a2, EPOCH, bind_request("c5", "t5", "/work/2")).await;
        assert_eq!(second.generation, generation);
        assert_eq!(
            second.base_commit, "c5",
            "a new generation binds its own base"
        );
        assert!(
            !second.populated,
            "a new binding never inherits a populated tree"
        );
    }

    fn plan_task(id: &str, detail: &str) -> PlanTask {
        PlanTask {
            task_id: id.into(),
            title: id.into(),
            detail: detail.into(),
            deps: vec![],
            acceptance: vec![],
            grant: crate::grant::WorkerGrant::minimal(),
            inputs: vec![],
            failing_test: None,
        }
    }

    #[tokio::test]
    async fn should_change_nothing_when_a_superseded_attempt_writes() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store).await;
        let stale = launch_running(&store, EPOCH).await;
        bound(&store, &stale, EPOCH, bind_request("c0", "t0", "/work/1")).await;
        let BeginVerification::Run { op_id, .. } = store
            .begin_verification("f1", "c1", &stale, EPOCH, run_request("c1", "t1"), 12)
            .await
            .unwrap()
        else {
            panic!("the live attempt records its intent");
        };

        // The host dies; a successor owns the task now.
        reboot(&store, EPOCH + 1).await;
        let current = launch_running(&store, EPOCH + 1).await;
        let before = store.get_child("f1", "c1").await.unwrap().unwrap();

        // Everything the dead attempt (or a process that outlived it) tries is
        // refused without touching the row.
        assert_eq!(
            store
                .bind_worktree(
                    "f1",
                    "c1",
                    &stale,
                    EPOCH,
                    bind_request("evil", "evil", "/work/evil"),
                    30
                )
                .await
                .unwrap(),
            BindWorktreeOutcome::Superseded,
        );
        assert_eq!(
            store
                .mark_worktree_populated("f1", "c1", &stale, EPOCH, "/work/evil", 30)
                .await
                .unwrap(),
            CompleteOutcome::Superseded,
        );
        assert_eq!(
            store
                .begin_verification("f1", "c1", &stale, EPOCH, run_request("c9", "t9"), 30)
                .await
                .unwrap(),
            BeginVerification::Superseded,
        );
        assert_eq!(
            store
                .commit_verification(
                    "f1",
                    "c1",
                    &stale,
                    EPOCH,
                    &op_id,
                    Some(0),
                    "late".into(),
                    30
                )
                .await
                .unwrap(),
            None,
            "a late result from a superseded attempt is dropped",
        );
        // The successor cannot commit the dead attempt's intent either.
        assert_eq!(
            store
                .commit_verification(
                    "f1",
                    "c1",
                    &current,
                    EPOCH + 1,
                    &op_id,
                    Some(0),
                    "forged".into(),
                    30,
                )
                .await
                .unwrap(),
            None,
            "an intent can only be completed by the attempt that started it",
        );

        assert_eq!(store.get_child("f1", "c1").await.unwrap().unwrap(), before);
    }

    #[tokio::test]
    async fn should_find_committed_result_when_same_run_is_looked_up_again() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store).await;
        let a1 = launch_running(&store, EPOCH).await;
        assert_eq!(
            store
                .find_verification("f1", "c1", &run_request("c1", "t1"))
                .await
                .unwrap(),
            None,
            "nothing has been run yet",
        );

        let BeginVerification::Run {
            op_id,
            unknown_prior,
        } = store
            .begin_verification("f1", "c1", &a1, EPOCH, run_request("c1", "t1"), 12)
            .await
            .unwrap()
        else {
            panic!("the live attempt records its intent");
        };
        assert_eq!(unknown_prior, 0);
        assert_eq!(
            store
                .find_verification("f1", "c1", &run_request("c1", "t1"))
                .await
                .unwrap(),
            None,
            "an intent with no outcome is not a verdict",
        );
        let committed = store
            .commit_verification("f1", "c1", &a1, EPOCH, &op_id, Some(0), "digest".into(), 15)
            .await
            .unwrap()
            .expect("the live attempt commits its outcome");
        assert_eq!(
            committed.result,
            Some(VerificationResult {
                exit_status: Some(0),
                output_sha256: "digest".into(),
                finished_at_ms: 15,
            }),
        );

        // The host dies after the test passed and before the task completed. The
        // successor looks the same run up and gets the committed receipt back —
        // and the lookup writes nothing.
        reboot(&store, EPOCH + 1).await;
        let a2 = launch_running(&store, EPOCH + 1).await;
        assert_eq!(
            store
                .find_verification("f1", "c1", &run_request("c1", "t1"))
                .await
                .unwrap(),
            Some(committed.clone()),
        );
        assert_eq!(verifications(&store).await, vec![committed.clone()]);

        // A run belongs to the attempt that started it.
        assert_eq!(
            store
                .commit_verification("f1", "c1", &a2, EPOCH + 1, &op_id, Some(1), "x".into(), 41)
                .await
                .unwrap(),
            None,
            "the successor did not start that run",
        );
        assert_eq!(verifications(&store).await, vec![committed]);
    }

    #[tokio::test]
    async fn should_permit_a_rerun_when_the_prior_result_is_unknown() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store).await;
        let a1 = launch_running(&store, EPOCH).await;
        let BeginVerification::Run { op_id: lost, .. } = store
            .begin_verification("f1", "c1", &a1, EPOCH, run_request("c1", "t1"), 12)
            .await
            .unwrap()
        else {
            panic!("the first run of a tree is executed");
        };

        // The host dies while the test runs: the intent is durable, the outcome
        // is not.
        reboot(&store, EPOCH + 1).await;
        let a2 = launch_running(&store, EPOCH + 1).await;
        let BeginVerification::Run {
            op_id: rerun,
            unknown_prior,
        } = store
            .begin_verification("f1", "c1", &a2, EPOCH + 1, run_request("c1", "t1"), 40)
            .await
            .unwrap()
        else {
            panic!("an unknown result permits a rerun on the same tree");
        };
        assert_eq!(unknown_prior, 1, "the lost run is visible as unknown");
        assert_ne!(rerun, lost);

        let records = verifications(&store).await;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].op_id, lost);
        assert_eq!(
            records[0].result, None,
            "the lost run stays recorded as unknown"
        );
        assert_eq!(records[1].attempt_id, a2);
    }

    #[tokio::test]
    async fn should_find_nothing_when_tree_commit_command_or_phase_differs() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store).await;
        let a1 = launch_running(&store, EPOCH).await;
        let BeginVerification::Run { op_id, .. } = store
            .begin_verification("f1", "c1", &a1, EPOCH, run_request("c1", "t1"), 12)
            .await
            .unwrap()
        else {
            panic!("first run");
        };
        store
            .commit_verification("f1", "c1", &a1, EPOCH, &op_id, Some(0), "d".into(), 13)
            .await
            .unwrap()
            .unwrap();

        let other_command = VerificationRequest {
            command: "cargo test other".into(),
            ..run_request("c1", "t1")
        };
        let other_criterion = VerificationRequest {
            criterion_id: "lint".into(),
            ..run_request("c1", "t1")
        };
        let other_phase = VerificationRequest {
            phase: VerificationPhase::Admission,
            ..run_request("c1", "t1")
        };
        for (name, request) in [
            ("another tree", run_request("c2", "t2")),
            ("another commit with the same tree", run_request("c2", "t1")),
            ("another command", other_command),
            ("another criterion", other_criterion),
            ("another phase", other_phase),
        ] {
            assert_eq!(
                store.find_verification("f1", "c1", &request).await.unwrap(),
                None,
                "[{name}] a receipt is never carried over to a different run",
            );
        }
        assert!(
            store
                .find_verification("f1", "c1", &run_request("c1", "t1"))
                .await
                .unwrap()
                .is_some(),
            "the exact run is found",
        );
    }

    #[tokio::test]
    async fn should_not_reuse_result_when_process_was_ended_by_a_signal() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store).await;
        let a1 = launch_running(&store, EPOCH).await;
        let BeginVerification::Run { op_id, .. } = store
            .begin_verification("f1", "c1", &a1, EPOCH, run_request("c1", "t1"), 12)
            .await
            .unwrap()
        else {
            panic!("first run");
        };
        store
            .commit_verification("f1", "c1", &a1, EPOCH, &op_id, None, "d".into(), 13)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            store
                .find_verification("f1", "c1", &run_request("c1", "t1"))
                .await
                .unwrap(),
            None,
            "an outcome without an exit code is not a verdict to reuse",
        );
    }

    #[tokio::test]
    async fn should_fail_when_marking_populated_without_a_binding() {
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store).await;
        let a1 = launch_running(&store, EPOCH).await;
        assert!(
            store
                .mark_worktree_populated("f1", "c1", &a1, EPOCH, "/work/1", 11)
                .await
                .is_err(),
        );
    }

    #[tokio::test]
    async fn should_write_no_worktree_fields_when_child_never_ran_in_a_worktree() {
        // A scratch task, or one that has not run yet, has no tree bound and no
        // acceptance run journaled: its row carries neither key.
        let (_d, store) = fresh().await;
        fleet_with_ready_child(&store).await;
        let child = store.get_child("f1", "c1").await.unwrap().unwrap();
        let json = serde_json::to_string(&child).unwrap();
        assert!(
            !json.contains("worktree") && !json.contains("verifications"),
            "got: {json}",
        );

        let decoded: FleetChildRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.worktree, None);
        assert!(decoded.verifications.is_empty());
        assert_eq!(decoded, child);
    }
}
