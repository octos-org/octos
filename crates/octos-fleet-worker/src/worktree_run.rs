//! How [`crate::run_attempt`] runs a task in a git worktree: bind, recover,
//! verify. This is the ONLY way a worktree attempt runs — there is no path that
//! resets a leftover checkout and starts over, and none that accepts a tree
//! without a receipt for it. (A scratch attempt has no tree and is not driven
//! from here.)
//!
//! **Admission happens before any edit.** The worktree and the IMMUTABLE base
//! revision are recorded in the fleet kernel. A task that names a failing test
//! ([`octos_fleet::FailingTest`]) must have a RECORDED run of that test failing
//! on that base before anything else happens — before a model is called, and
//! before recovered work is even looked at. When no such record exists (the
//! first attempt, or a run whose result was lost with its host) the test is run
//! on the base revision itself. A test that passes there is not a failing test
//! to fix: the attempt is refused and no model is called.
//!
//! **Work survives the host.** A later attempt of the same generation is
//! handed the dead attempt's directory, and whatever it holds is committed to
//! the task branch as a snapshot; it is never reset. That holds wherever the
//! dead attempt left its `HEAD` — detached, or on a branch of its own — because
//! the snapshot is made from the directory's content, not from `HEAD`. A
//! recorded directory git can no longer commit from (its `.git` file is gone,
//! say) is not recovered and not removed either: the task is not dispatched
//! until somebody has looked at it. With an admitted failing test the recovered
//! commit is VERIFIED FIRST: if the acceptance commands already pass on it, the
//! task completes with zero model calls. Otherwise — the commit does not pass,
//! or the task admitted no test and so has nothing that says the work is
//! finished — the agent continues from the recovered work instead of starting
//! over.
//!
//! **A dead attempt's processes are left behind, not followed.** Every attempt
//! works in a directory of its own. The lease fences a dead attempt's store
//! writes, not its files: a build or a script it started may still be running
//! and writing. So the successor never works in the dead attempt's directory.
//! It commits what that directory holds and continues in a new directory
//! populated from the branch — a path the old process was never given, outside
//! what its sandbox lets it write. Once the new directory is recorded as the
//! one holding the work, the old one is retired: removed and unregistered, so
//! git no longer works there. In that order the record never names a directory
//! that has been removed.
//!
//! **A verdict is about a commit, and nothing else.** No check ever runs in the
//! attempt's own checkout. The tree under test is always a committed tree whose
//! id is read host-side, and its commands run in a throwaway checkout of
//! exactly that commit that is created for the run and removed after it. So a
//! file git ignores — a build cache, a marker an earlier run left — cannot make
//! a commit pass that would fail where it is delivered, and a test that
//! rewrites files rewrites the throwaway copy, never the work. A `FileExists`
//! criterion is answered from the commit too.
//!
//! That checkout is a repository of its own
//! ([`octos_core::clean_checkout_command`]): it borrows the task repository's
//! objects and reads none of its configuration, attribute files or hooks —
//! all of which a worker can write. A filter planted there to rewrite a file
//! as it is checked out therefore changes nothing: the files tested are the
//! commit's own.
//!
//! **Every command leaves a receipt bound to that commit.** The intent to run
//! is recorded before the command starts, and its exit status and output digest
//! after. Repeating a local test is always allowed; NOT repeating it is an
//! optimization, taken only when every acceptance command already has a
//! committed result for the exact commit (one of them failing settles it too).
//! Otherwise all of them run again, in order, in one fresh checkout — a later
//! command may depend on what an earlier one set up. A tree with no change
//! relative to the base is no deliverable at all.
//!
//! # What this does NOT do yet
//!
//! - **It does not kill a process that outlived its host.** Such a process is
//!   isolated (above), not terminated: it keeps running until it ends by
//!   itself. What it wrote before the takeover snapshot is recovered as work
//!   like anything else in that directory. And isolation is as strong as the
//!   sandbox: a process that writes refs straight into the shared `.git`
//!   directory is not stopped. A branch that moved while a commit was being
//!   verified is verified again or not delivered; one moved after that simply
//!   no longer points at the commit the receipt names.
//! - **A clean checkout has no build cache.** Every verification builds from
//!   nothing, which is slow for a compiled project.
//! - **A clean checkout has none of the repository's local setup.** A file the
//!   repository stores through a locally configured filter (Git LFS) is checked
//!   out as the commit holds it — the pointer — and none of the repository's
//!   branches can be named there. The OBJECTS are still shared: a process that
//!   rewrites the object store itself is not stopped.
//! - **Only the acceptance commands are journaled.** The agent's own tool calls
//!   inside a turn are not recorded one operation at a time.
//! - **A recorded result is not questioned.** A test that passes once on the
//!   base by accident refuses the task for that generation of the plan
//!   ("no failing test to fix"), and one that fails once on a commit by
//!   accident is reused as that commit's rejection. A flaky test is not
//!   detected.
//! - **The directory of a task that ended without being accepted stays on
//!   disk** until the task is dispatched again, which may never happen.
//!   Nothing else removes it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use octos_agent::policy::{CommandPolicy, Decision, SafePolicy};
use octos_agent::sandbox::Sandbox;
use octos_fleet::{
    AcceptanceVerdict, BeginVerification, BindWorktreeOutcome, CompleteOutcome, EvidenceRef,
    FailingTest, FleetKernelStore, TaskView, VerificationPhase, VerificationRecord,
    VerificationRequest, Verifier, WorktreeBindRequest, WorktreeBinding,
};
use sha2::{Digest, Sha256};

use crate::worker::{
    AttemptOutcome, WorktreeContext, populate_worktree, run_sandboxed_git, sandboxed_git_command,
};
use crate::{AgentFactory, SandboxGrant};

/// How many commits of one attempt may be verified before it is rejected as
/// unstable. The checks never touch the attempt's checkout, so the tree only
/// moves under a verification when something else is still writing there; one
/// more round absorbs a writer that was just finishing.
const MAX_VERIFICATION_RUNS: usize = 2;

/// Exit codes a POSIX shell uses for "found but not executable" and "not found".
/// On the base revision they mean the admitted command cannot run at all, which
/// is not the same thing as a failing test.
const EXIT_CANNOT_EXECUTE: i32 = 126;
const EXIT_NOT_FOUND: i32 = 127;

/// A point at which a test can stop an attempt dead, as if the host had been
/// killed there: nothing after the point runs, and only what was already
/// committed to the store or the repository survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum KillPoint {
    /// The admitted test has exited on the base revision; its outcome is not
    /// committed yet.
    AdmissionCommandExited,
    /// An acceptance command has exited on the candidate commit; its outcome
    /// is not committed yet.
    AcceptanceCommandExited,
    /// The attempt's verdict is computed; the completion is not committed yet.
    BeforeCompletion,
    /// Recovered work is committed and this attempt's directory is recorded as
    /// holding it; the interrupted attempt's directory is not retired yet.
    BeforeRetiringDead,
}

/// Stop here forever if a test armed `point` for `store`. Compiles to nothing
/// outside tests.
#[cfg(not(test))]
pub(crate) async fn kill_point(_store: &Arc<FleetKernelStore>, _point: KillPoint) {}

#[cfg(test)]
pub(crate) use kill_points::hit as kill_point;

#[cfg(test)]
pub(crate) mod kill_points {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};

    use octos_fleet::FleetKernelStore;
    use tokio::sync::oneshot;

    use super::KillPoint;

    /// A store instance and a point in an attempt running against it.
    type Key = (usize, KillPoint);

    /// An armed point: how many more arrivals go through before one is stopped,
    /// and whom to tell when it is.
    struct Armed {
        passes: usize,
        reached: oneshot::Sender<()>,
    }

    /// Armed kill points, keyed by the store INSTANCE so concurrently running
    /// tests (each with its own store) never trip one another's.
    static ARMED: LazyLock<Mutex<HashMap<Key, Armed>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn key(store: &Arc<FleetKernelStore>, point: KillPoint) -> Key {
        (Arc::as_ptr(store) as usize, point)
    }

    /// Arm `point` once for `store`. The receiver resolves when an attempt
    /// reaches the point — and that attempt never proceeds past it.
    pub(crate) fn arm(store: &Arc<FleetKernelStore>, point: KillPoint) -> oneshot::Receiver<()> {
        arm_after(store, point, 0)
    }

    /// Like [`arm`], but the first `passes` arrivals at `point` go through and
    /// the one after them is stopped.
    pub(crate) fn arm_after(
        store: &Arc<FleetKernelStore>,
        point: KillPoint,
        passes: usize,
    ) -> oneshot::Receiver<()> {
        let (reached, rx) = oneshot::channel();
        ARMED
            .lock()
            .unwrap()
            .insert(key(store, point), Armed { passes, reached });
        rx
    }

    pub(crate) async fn hit(store: &Arc<FleetKernelStore>, point: KillPoint) {
        let reached = {
            let mut armed = ARMED.lock().unwrap();
            let key = key(store, point);
            match armed.get_mut(&key) {
                Some(point) if point.passes > 0 => {
                    point.passes -= 1;
                    None
                }
                Some(_) => armed.remove(&key).map(|point| point.reached),
                None => None,
            }
        };
        if let Some(reached) = reached {
            let _ = reached.send(());
            std::future::pending::<()>().await;
        }
    }
}

/// The attempt cannot continue and its outcome is already decided: a store
/// fence was lost (the attempt is not ours any more) or the store itself failed.
/// [`crate::run_attempt`] returns the outcome unchanged.
pub(crate) struct Stop(pub(crate) AttemptOutcome);

/// What admission decided.
pub(crate) enum Admission {
    /// Run the agent. `resume` is appended to its brief when it is continuing
    /// recovered work rather than starting from the base.
    RunAgent {
        base: WorktreeBinding,
        resume: Option<String>,
    },
    /// The attempt is settled without calling a model.
    Settled {
        verdict: AcceptanceVerdict,
        error: Option<String>,
        output: String,
    },
}

impl Admission {
    fn terminated(reason: String) -> Self {
        Admission::Settled {
            verdict: AcceptanceVerdict::Terminated {
                reason: reason.clone(),
            },
            error: Some(reason),
            output: String::new(),
        }
    }

    fn rejected(reason: String) -> Self {
        Admission::Settled {
            verdict: AcceptanceVerdict::Rejected {
                reason: reason.clone(),
            },
            error: Some(reason),
            output: String::new(),
        }
    }
}

/// A throwaway checkout of exactly one commit, and the sandbox that confines
/// that commit's commands to it.
struct CleanCheckout {
    dir: PathBuf,
    sandbox: Arc<dyn Sandbox>,
}

/// One command run against one commit.
enum Checked {
    /// The run's committed receipt.
    Receipt(VerificationRecord),
    /// The command outlived the attempt's remaining time and was killed. Its
    /// intent stays in the journal with no result.
    DeadlineExceeded,
    /// The command could not be started.
    CannotRun(String),
}

/// The acceptance criteria checked against one tree.
enum Criteria {
    Passed(Vec<EvidenceRef>),
    Rejected(String),
    Terminated(String),
}

/// One worktree attempt: the ids and handles every step needs.
pub(crate) struct WorktreeRun<'a, C: Fn() -> u64> {
    pub(crate) store: &'a Arc<FleetKernelStore>,
    pub(crate) fleet_id: &'a str,
    pub(crate) task_id: &'a str,
    pub(crate) attempt_id: &'a str,
    pub(crate) owner_epoch: u64,
    pub(crate) task_view: &'a TaskView,
    /// The failing test the task was admitted on, when it names one.
    pub(crate) admitted_test: Option<&'a FailingTest>,
    pub(crate) worktree: &'a WorktreeContext,
    /// The sandbox confined to the attempt's own checkout: populate and commit
    /// run here. No check does.
    pub(crate) sandbox: &'a Arc<dyn Sandbox>,
    pub(crate) checkout: &'a Path,
    /// Makes the sandbox for each clean checkout, with the same `grant` the
    /// attempt's own sandbox was made from.
    pub(crate) factory: &'a AgentFactory,
    pub(crate) grant: &'a SandboxGrant,
    /// Numbers this attempt's clean checkouts, so each gets its own directory.
    pub(crate) clean_checkouts: AtomicUsize,
    pub(crate) clock: &'a C,
}

impl<C: Fn() -> u64> WorktreeRun<'_, C> {
    /// Bind the base, bring the checkout to a known tree, and decide whether a
    /// model needs to be called at all. See the module docs.
    pub(crate) async fn admit(&self, budget: Duration) -> Result<Admission, Stop> {
        let started = Instant::now();
        let remaining = |started: Instant| budget.saturating_sub(started.elapsed());

        // The branch as the pool prepared it. For the first attempt of a
        // generation this is the base; later attempts read the base back.
        let (head_commit, head_tree) = match self.head().await {
            Ok(head) => head,
            Err(reason) => return Ok(Admission::terminated(reason)),
        };
        let base = match self
            .store
            .bind_worktree(
                self.fleet_id,
                self.task_id,
                self.attempt_id,
                self.owner_epoch,
                WorktreeBindRequest {
                    repo_root: self.worktree.repo_root.display().to_string(),
                    branch: self.worktree.branch.clone(),
                    checkout: self.checkout.display().to_string(),
                    base_commit: head_commit,
                    base_tree: head_tree,
                },
                (self.clock)(),
            )
            .await
        {
            Ok(BindWorktreeOutcome::Bound(binding)) => binding,
            Ok(BindWorktreeOutcome::Superseded) => return Err(Stop(AttemptOutcome::Superseded)),
            Err(err) => return Err(self.record_error("bind_worktree", &err)),
        };

        // Admission proper. It is decided on the base revision alone — whatever
        // the checkout holds — so it comes before the checkout is touched.
        let mut admission_budget = remaining(started);
        if let Some(refused) = self.admission(&base, &mut admission_budget).await? {
            return Ok(refused);
        }

        let dead = self.worktree.recover_from.as_deref();
        if let Some(dead) = dead {
            // The interrupted attempt's directory. Commit what it holds; this
            // attempt never works where that attempt's processes may still be
            // writing.
            if let Some(reason) = self.snapshot_dead(dead, &base, remaining(started)).await {
                return Ok(Admission::terminated(reason));
            }
        }

        // This attempt's own checkout, which the controller created empty:
        // restore it from the branch — which now carries anything recovered —
        // and only then record it as the directory that holds the work.
        if let Some(reason) =
            populate_worktree(self.sandbox, self.checkout, remaining(started)).await
        {
            return Ok(Admission::terminated(reason));
        }
        match self
            .store
            .mark_worktree_populated(
                self.fleet_id,
                self.task_id,
                self.attempt_id,
                self.owner_epoch,
                &self.checkout.display().to_string(),
                (self.clock)(),
            )
            .await
        {
            Ok(CompleteOutcome::Completed) => {}
            Ok(CompleteOutcome::Superseded) => return Err(Stop(AttemptOutcome::Superseded)),
            Err(err) => return Err(self.record_error("mark_worktree_populated", &err)),
        }
        if let Some(dead) = dead {
            // The record names this attempt's directory now, so the dead one
            // is no longer where the work is: it can go.
            kill_point(self.store, KillPoint::BeforeRetiringDead).await;
            self.retire(dead).await;
        }

        let (commit, tree) = match self.head().await {
            Ok(head) => head,
            Err(reason) => return Ok(Admission::terminated(reason)),
        };

        if tree == base.base_tree {
            // Nothing to recover: the agent starts from the base.
            return Ok(Admission::RunAgent { base, resume: None });
        }

        // Work exists — committed by an earlier attempt, or snapshotted just
        // now. The agent, if it runs, continues from it.
        let resume = |state: &str| {
            format!(
                "\n## Resuming interrupted work\nAn earlier attempt at this task was interrupted. \
                 Its work was recovered and is committed on this branch (base revision `{base}`, \
                 recovered head `{commit}`). Inspect it with `git diff {base} HEAD` and continue \
                 from it; do not start over. {state}\n",
                base = base.base_commit,
            )
        };

        if self.admitted_test.is_none() {
            // Without an admitted failing test the acceptance criteria do not
            // by themselves say the work is FINISHED (a task may have none at
            // all), and the interrupted attempt never said so either. Recovered
            // work is therefore never delivered unseen: the agent resumes it.
            tracing::info!(
                fleet_id = %self.fleet_id, task_id = %self.task_id, attempt_id = %self.attempt_id,
                %commit, base = %base.base_commit,
                "fleet worker: recovered interrupted work; the agent resumes from it",
            );
            let note = resume("It has not been checked against acceptance.");
            return Ok(Admission::RunAgent {
                base,
                resume: Some(note),
            });
        }

        // The admitted test has a recorded failure on the base (`admission`
        // above). Verify the recovered commit before any model call: if the
        // test and every other criterion pass on it, the work is done.
        tracing::info!(
            fleet_id = %self.fleet_id, task_id = %self.task_id, attempt_id = %self.attempt_id,
            %commit, base = %base.base_commit,
            "fleet worker: recovered interrupted work; verifying it before any model call",
        );
        match self.verify(&base, remaining(started)).await? {
            (verdict @ AcceptanceVerdict::Accepted { .. }, _) => Ok(Admission::Settled {
                verdict,
                error: None,
                output: format!(
                    "Recovered the interrupted attempt's work at commit {commit} and verified it \
                     against base {}; no model was called.",
                    base.base_commit
                ),
            }),
            (AcceptanceVerdict::Rejected { reason }, _) => {
                let note = resume(&format!(
                    "The recovered tree does not pass acceptance yet: {reason}"
                ));
                Ok(Admission::RunAgent {
                    base,
                    resume: Some(note),
                })
            }
            (verdict @ AcceptanceVerdict::Terminated { .. }, error) => Ok(Admission::Settled {
                verdict,
                error,
                output: String::new(),
            }),
        }
    }

    /// Secure the work in `dead`, an interrupted attempt's directory: commit
    /// whatever it holds to the task branch. `Some(reason)` when that could
    /// not be done — the attempt then terminates and `dead` is left exactly as
    /// it was.
    ///
    /// The commit is strict (a snapshot that did not commit leaves the work
    /// only in `dead`), and it does not depend on where `dead`'s `HEAD` points:
    /// see [`octos_core::deliverable_commit_command`].
    async fn snapshot_dead(
        &self,
        dead: &Path,
        base: &WorktreeBinding,
        budget: Duration,
    ) -> Option<String> {
        if !base.populated || Path::new(&base.checkout) != dead {
            return Some(format!(
                "the directory to recover ({}) is not the one recorded as holding the work ({})",
                dead.display(),
                base.checkout
            ));
        }
        let failed =
            |reason: String| Some(format!("could not snapshot the recovered work: {reason}"));
        let sandbox = match self.sandbox_for(dead) {
            Ok(sandbox) => sandbox,
            Err(reason) => return failed(reason),
        };
        let message = format!("fleet {} recovered work", self.task_id);
        let command = match octos_core::recovery_snapshot_command(&self.worktree.branch, &message) {
            Ok(command) => command,
            Err(err) => return failed(format!("{err:#}")),
        };
        match run_sandboxed_git(&sandbox, dead, &command, budget, true).await {
            Ok(()) => None,
            Err(err) => failed(err),
        }
    }

    /// Retire `dead`, an interrupted attempt's directory whose work is
    /// committed and which the record no longer names: remove it and its
    /// worktree registration. A process the dead attempt left running loses
    /// its directory, and a `git` command it runs there no longer finds a
    /// repository. From here on that process and this attempt share no working
    /// directory.
    async fn retire(&self, dead: &Path) {
        let repo_root = self.worktree.repo_root.clone();
        let work_root = self.worktree.work_root.clone();
        let retired = dead.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            octos_core::remove_checkout_keep_branch(&repo_root, &work_root, &retired);
        })
        .await;
        if dead.exists() {
            // Something is still creating files there. The directory is
            // unregistered (git does not work in it) and nothing reads it
            // again; it is removed when the task is next dispatched.
            tracing::warn!(
                fleet_id = %self.fleet_id, task_id = %self.task_id, dead = %dead.display(),
                "fleet worker: the interrupted attempt's directory could not be fully removed; \
                 something is still writing there",
            );
        }
        tracing::info!(
            fleet_id = %self.fleet_id, task_id = %self.task_id, attempt_id = %self.attempt_id,
            dead = %dead.display(), checkout = %self.checkout.display(),
            "fleet worker: took over the interrupted attempt's work; continuing in a new directory",
        );
    }

    /// A sandbox confined to `dir`, made by the same factory from the same
    /// grant as the attempt's own. One that does not isolate, or cannot run the
    /// worktree flow, is refused.
    fn sandbox_for(&self, dir: &Path) -> Result<Arc<dyn Sandbox>, String> {
        let sandbox = self.factory.sandbox_for(dir, self.grant.clone());
        let usable = !sandbox.is_noop()
            && sandbox.refusal().is_none()
            && sandbox.supports_repo_git_write()
            && sandbox.runs_posix_sh();
        if usable {
            Ok(sandbox)
        } else {
            Err(format!(
                "no isolating sandbox that can run the worktree flow is available for {}",
                dir.display()
            ))
        }
    }

    /// Admission proper: a task that names a failing test may only proceed on a
    /// RECORDED run of that test failing on the base revision.
    ///
    /// `Ok(None)` admits the attempt — the task names no test, or the record
    /// exists (found, or made just now). `Ok(Some(_))` settles it: the test
    /// passes on the base (nothing to fix), cannot be run there, or could not
    /// be checked in the time left.
    ///
    /// The record is looked up first, so a successor does not repeat a result
    /// its predecessor committed. When there is none — the first attempt, or a
    /// run whose result was lost with its host — the test runs in a clean
    /// checkout of the base commit itself, never in the attempt's checkout:
    /// what the checkout holds (including anything a lost run of this very
    /// test wrote there) has no say in whether the base was failing.
    async fn admission(
        &self,
        base: &WorktreeBinding,
        budget: &mut Duration,
    ) -> Result<Option<Admission>, Stop> {
        let (criterion_id, cmd) = match self.admitted_test_command() {
            Ok(Some(named)) => named,
            Ok(None) => return Ok(None),
            Err(reason) => return Ok(Some(Admission::rejected(reason))),
        };
        let request = VerificationRequest {
            phase: VerificationPhase::Admission,
            criterion_id: criterion_id.to_string(),
            command: cmd.to_string(),
            commit: base.base_commit.clone(),
            tree_digest: base.base_tree.clone(),
        };
        let receipt = match self.find(&request).await? {
            Some(receipt) => receipt,
            None => {
                let clean = match self.open_clean(&base.base_commit, budget).await {
                    Ok(clean) => clean,
                    Err(reason) => {
                        return Ok(Some(Admission::terminated(format!(
                            "could not check out base revision {} to run the admitted test: \
                             {reason}",
                            base.base_commit
                        ))));
                    }
                };
                let checked = self.run_recorded(&clean, &request, budget).await;
                self.close_clean(clean).await;
                match checked? {
                    Checked::Receipt(receipt) => receipt,
                    Checked::DeadlineExceeded => {
                        return Ok(Some(Admission::terminated(format!(
                            "deadline exceeded while running the admitted test `{cmd}` on base \
                             revision {}",
                            base.base_commit
                        ))));
                    }
                    Checked::CannotRun(reason) => {
                        return Ok(Some(Admission::terminated(format!(
                            "the admitted test `{cmd}` could not be started: {reason}"
                        ))));
                    }
                }
            }
        };
        match receipt.result.as_ref().and_then(|r| r.exit_status) {
            Some(0) => Ok(Some(Admission::rejected(format!(
                "admission refused: `{cmd}` already passes on base revision {} — there is no \
                 failing test to fix",
                base.base_commit
            )))),
            Some(code @ (EXIT_CANNOT_EXECUTE | EXIT_NOT_FOUND)) => {
                Ok(Some(Admission::rejected(format!(
                    "admission refused: `{cmd}` cannot be executed on base revision {} (exit \
                     {code})",
                    base.base_commit
                ))))
            }
            // A non-zero exit, or a crash: the test fails on the base.
            _ => Ok(None),
        }
    }

    /// Verify the work in the checkout and return the attempt's verdict.
    ///
    /// The checkout is committed first (strictly: a snapshot that fails leaves
    /// the tree unknown, and the attempt terminates), and it is that COMMIT
    /// which is checked — in a clean checkout of its own, see
    /// [`Self::check_criteria`]. Afterwards the checkout is committed again;
    /// the verdict is `Accepted` only if that produces the same tree, so the
    /// branch head delivered is exactly the commit that was verified. The
    /// checks cannot move it (they do not run there); if something else did,
    /// the new commit is verified in turn, at most [`MAX_VERIFICATION_RUNS`]
    /// times in all.
    pub(crate) async fn verify(
        &self,
        base: &WorktreeBinding,
        mut budget: Duration,
    ) -> Result<(AcceptanceVerdict, Option<String>), Stop> {
        let message = format!("fleet {} deliverable", self.task_id);
        let snapshot = match octos_core::recovery_snapshot_command(&self.worktree.branch, &message)
        {
            Ok(snapshot) => snapshot,
            Err(err) => {
                return Ok(terminated(format!(
                    "could not commit the worktree for verification: {err:#}"
                )));
            }
        };
        let mut verified: Option<(String, Vec<EvidenceRef>)> = None;

        for run in 0..=MAX_VERIFICATION_RUNS {
            if budget.is_zero() {
                return Ok(terminated("acceptance deadline exceeded".to_string()));
            }
            let started = Instant::now();
            let committed =
                run_sandboxed_git(self.sandbox, self.checkout, &snapshot, budget, true).await;
            budget = budget.saturating_sub(started.elapsed());
            if let Err(err) = committed {
                return Ok(terminated(format!(
                    "could not commit the worktree for verification: {err}"
                )));
            }
            let (commit, tree) = match self.head().await {
                Ok(head) => head,
                Err(reason) => return Ok(terminated(reason)),
            };

            if let Some((tested_tree, evidence)) = &verified {
                if *tested_tree == tree {
                    return Ok((
                        AcceptanceVerdict::Accepted {
                            evidence: evidence.clone(),
                        },
                        None,
                    ));
                }
            }
            if run == MAX_VERIFICATION_RUNS {
                break;
            }
            if tree == base.base_tree {
                return Ok(rejected(format!(
                    "no changes relative to base revision {} for task {} — there is no patch to \
                     verify or deliver",
                    base.base_commit, self.task_id
                )));
            }

            match self.check_criteria(&commit, &tree, &mut budget).await? {
                Criteria::Passed(evidence) => verified = Some((tree, evidence)),
                Criteria::Rejected(reason) => return Ok(rejected(reason)),
                Criteria::Terminated(reason) => return Ok(terminated(reason)),
            }
        }

        Ok(rejected(format!(
            "the worktree kept changing while it was being verified; no receipt can be bound to \
             the commit that would be delivered for task {}",
            self.task_id
        )))
    }

    /// Check every acceptance criterion against the commit `commit` (whose tree
    /// is `tree`). Fail-closed like the ordinary gate: a criterion the executor
    /// cannot check mechanically rejects.
    ///
    /// Nothing here looks at the attempt's checkout. A `FileExists` criterion
    /// asks whether the COMMIT holds that file. The commands run in a clean
    /// checkout of the commit — unless every one of them already has a
    /// committed result for it, in which case none runs (see the module docs
    /// for why a partial set is not reused).
    async fn check_criteria(
        &self,
        commit: &str,
        tree: &str,
        budget: &mut Duration,
    ) -> Result<Criteria, Stop> {
        let mut evidence = Vec::new();
        let mut commands = Vec::new();
        for criterion in &self.task_view.acceptance {
            match &criterion.verifier {
                Verifier::FileExists { path } => match self.commit_has_file(commit, path).await {
                    Ok(true) => evidence.push(EvidenceRef {
                        kind: "file_exists".to_string(),
                        locator: format!("{commit}:{path}"),
                        sha256: String::new(),
                        captured_at_ms: 0,
                    }),
                    Ok(false) => {
                        return Ok(Criteria::Rejected(format!(
                            "acceptance failed — {}: commit {commit} holds no regular file \
                             `{path}`",
                            criterion.id
                        )));
                    }
                    Err(reason) => {
                        return Ok(Criteria::Rejected(format!(
                            "acceptance failed — {}: {reason}",
                            criterion.id
                        )));
                    }
                },
                Verifier::CommandExit { cmd, code: 0 } if !cmd.trim().is_empty() => {
                    commands.push(VerificationRequest {
                        phase: VerificationPhase::Acceptance,
                        criterion_id: criterion.id.clone(),
                        command: cmd.clone(),
                        commit: commit.to_string(),
                        tree_digest: tree.to_string(),
                    });
                }
                other => {
                    return Ok(Criteria::Rejected(format!(
                        "acceptance cannot be verified — {}: unsupported verifier {other:?}",
                        criterion.id
                    )));
                }
            }
        }
        if commands.is_empty() {
            return Ok(Criteria::Passed(evidence));
        }

        // What is already committed for this exact commit, in order. A failure
        // settles it; a full set of passes settles it; a gap means run them all.
        let mut committed = Vec::with_capacity(commands.len());
        for request in &commands {
            match self.find(request).await? {
                Some(receipt) if receipt_passed(&receipt) => committed.push(receipt),
                Some(receipt) => return Ok(Criteria::Rejected(failure(request, &receipt))),
                None => break,
            }
        }
        if committed.len() == commands.len() {
            tracing::info!(
                fleet_id = %self.fleet_id, task_id = %self.task_id, %commit,
                "fleet worker: every acceptance command already has a committed result for \
                 this commit; not running them again",
            );
            evidence.extend(committed.iter().map(receipt_evidence));
            return Ok(Criteria::Passed(evidence));
        }

        let clean = match self.open_clean(commit, budget).await {
            Ok(clean) => clean,
            Err(reason) => {
                return Ok(Criteria::Terminated(format!(
                    "could not check out commit {commit} for verification: {reason}"
                )));
            }
        };
        let outcome = self.run_commands(&clean, &commands, budget).await;
        self.close_clean(clean).await;
        Ok(match outcome? {
            Criteria::Passed(receipts) => {
                evidence.extend(receipts);
                Criteria::Passed(evidence)
            }
            other => other,
        })
    }

    /// Run `commands` in order in `clean`, each through the journal, stopping
    /// at the first that does not pass.
    async fn run_commands(
        &self,
        clean: &CleanCheckout,
        commands: &[VerificationRequest],
        budget: &mut Duration,
    ) -> Result<Criteria, Stop> {
        let mut evidence = Vec::with_capacity(commands.len());
        for request in commands {
            let receipt = match self.run_recorded(clean, request, budget).await? {
                Checked::Receipt(receipt) => receipt,
                Checked::DeadlineExceeded => {
                    return Ok(Criteria::Terminated(
                        "acceptance deadline exceeded".to_string(),
                    ));
                }
                Checked::CannotRun(reason) => {
                    return Ok(Criteria::Rejected(format!(
                        "acceptance failed — {}: `{}` could not be run: {reason}",
                        request.criterion_id, request.command
                    )));
                }
            };
            if !receipt_passed(&receipt) {
                return Ok(Criteria::Rejected(failure(request, &receipt)));
            }
            evidence.push(receipt_evidence(&receipt));
        }
        Ok(Criteria::Passed(evidence))
    }

    /// Run one command in `clean` through the journal: record the intent, run
    /// it in the clean checkout's sandbox, commit its outcome.
    async fn run_recorded(
        &self,
        clean: &CleanCheckout,
        request: &VerificationRequest,
        budget: &mut Duration,
    ) -> Result<Checked, Stop> {
        let begun = self
            .store
            .begin_verification(
                self.fleet_id,
                self.task_id,
                self.attempt_id,
                self.owner_epoch,
                request.clone(),
                (self.clock)(),
            )
            .await;
        let op_id = match begun {
            Ok(BeginVerification::Run {
                op_id,
                unknown_prior,
            }) => {
                if unknown_prior > 0 {
                    tracing::warn!(
                        fleet_id = %self.fleet_id, task_id = %self.task_id,
                        criterion_id = %request.criterion_id, commit = %request.commit,
                        unknown_prior,
                        "fleet worker: an earlier run on this commit has no recorded result; \
                         running the repeatable command again in a clean checkout",
                    );
                }
                op_id
            }
            Ok(BeginVerification::Superseded) => return Err(Stop(AttemptOutcome::Superseded)),
            Err(err) => return Err(self.record_error("begin_verification", &err)),
        };

        let started = Instant::now();
        let ran = run_command(&clean.sandbox, &clean.dir, &request.command, *budget).await;
        *budget = budget.saturating_sub(started.elapsed());
        let output = match ran {
            Ok(output) => output,
            Err(CommandFailure::TimedOut) => return Ok(Checked::DeadlineExceeded),
            Err(CommandFailure::CannotRun(reason)) => return Ok(Checked::CannotRun(reason)),
        };

        let exited = match request.phase {
            VerificationPhase::Admission => KillPoint::AdmissionCommandExited,
            VerificationPhase::Acceptance => KillPoint::AcceptanceCommandExited,
        };
        kill_point(self.store, exited).await;

        match self
            .store
            .commit_verification(
                self.fleet_id,
                self.task_id,
                self.attempt_id,
                self.owner_epoch,
                &op_id,
                output.exit_status,
                output.sha256,
                (self.clock)(),
            )
            .await
        {
            Ok(Some(receipt)) => Ok(Checked::Receipt(receipt)),
            Ok(None) => Err(Stop(AttemptOutcome::Superseded)),
            Err(err) => Err(self.record_error("commit_verification", &err)),
        }
    }

    /// The result already committed for exactly `request`, if any.
    async fn find(
        &self,
        request: &VerificationRequest,
    ) -> Result<Option<VerificationRecord>, Stop> {
        self.store
            .find_verification(self.fleet_id, self.task_id, request)
            .await
            .map_err(|err| self.record_error("find_verification", &err))
    }

    /// Create a clean checkout of exactly `commit`, next to the attempt's own
    /// checkout, and fill it inside a sandbox confined to it.
    ///
    /// The directory is new for every call (`<task>.verify-<attempt>-<n>`), so
    /// nothing an earlier run wrote can be in it, and a process that outlived
    /// an earlier attempt does not know its name. It is filled as a repository
    /// of its own that borrows the task repository's objects and reads none of
    /// its configuration ([`octos_core::clean_checkout_command`]), so its files
    /// are the commit's own whatever a worker wrote into the shared `.git`. The
    /// sandbox is made by the same factory, from the same grant, as the
    /// attempt's own; one that does not isolate, or cannot run the worktree
    /// flow, is refused.
    async fn open_clean(
        &self,
        commit: &str,
        budget: &mut Duration,
    ) -> Result<CleanCheckout, String> {
        let number = self.clean_checkouts.fetch_add(1, Ordering::Relaxed);
        let label = format!("{}-{number}", octos_core::safe_filename(self.attempt_id));
        let dir = octos_core::clean_checkout_path(&self.worktree.stem, &label)
            .map_err(|err| format!("{err:#}"))?;
        let fill = octos_core::clean_checkout_command(&self.worktree.git_dir, commit)
            .map_err(|err| format!("{err:#}"))?;

        let started = Instant::now();
        let prepared = {
            let work_root = self.worktree.work_root.clone();
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || {
                octos_core::prepare_clean_checkout_dir(&work_root, &dir)
            })
            .await
        };
        *budget = budget.saturating_sub(started.elapsed());
        match prepared {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(format!("{err:#}")),
            Err(err) => return Err(err.to_string()),
        }

        let sandbox = match self.sandbox_for(&dir) {
            Ok(sandbox) => sandbox,
            Err(reason) => {
                self.remove_clean(dir).await;
                return Err(reason);
            }
        };
        let started = Instant::now();
        let filled = run_sandboxed_git(&sandbox, &dir, &fill, *budget, true).await;
        *budget = budget.saturating_sub(started.elapsed());
        if let Err(reason) = filled {
            self.remove_clean(dir).await;
            return Err(format!("clean checkout failed: {reason}"));
        }
        Ok(CleanCheckout { dir, sandbox })
    }

    /// Remove a clean checkout once its commands have run.
    async fn close_clean(&self, clean: CleanCheckout) {
        self.remove_clean(clean.dir).await;
    }

    /// Remove a clean checkout directory. Best-effort: one that is left behind
    /// is removed when the task is next dispatched.
    async fn remove_clean(&self, dir: PathBuf) {
        let repo_root = self.worktree.repo_root.clone();
        let work_root = self.worktree.work_root.clone();
        let _ = tokio::task::spawn_blocking(move || {
            octos_core::remove_checkout_keep_branch(&repo_root, &work_root, &dir);
        })
        .await;
    }

    /// Whether `commit` holds a regular file at `path`, read host-side.
    async fn commit_has_file(&self, commit: &str, path: &str) -> Result<bool, String> {
        let repo_root = self.worktree.repo_root.clone();
        let (commit, path) = (commit.to_string(), path.to_string());
        tokio::task::spawn_blocking(move || {
            octos_core::commit_has_regular_file(&repo_root, &commit, &path)
        })
        .await
        .map_err(|err| format!("reading the commit failed: {err}"))?
        .map_err(|err| format!("{err:#}"))
    }

    /// The criterion id and command of the admitted failing test: `Ok(None)`
    /// when the task names none, `Err` when it names a criterion that is not a
    /// runnable `CommandExit(0)` (plan validation refuses such a plan, so this
    /// is a stored plan that predates or bypassed it).
    fn admitted_test_command(&self) -> Result<Option<(&str, &str)>, String> {
        let Some(named) = self.admitted_test else {
            return Ok(None);
        };
        self.task_view
            .acceptance
            .iter()
            .find(|c| c.id == named.criterion)
            .and_then(|c| match &c.verifier {
                Verifier::CommandExit { cmd, code: 0 } if !cmd.trim().is_empty() => {
                    Some((c.id.as_str(), cmd.as_str()))
                }
                _ => None,
            })
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "admission refused: the task names failing test criterion {:?}, which is not \
                     a CommandExit(0) criterion of this task",
                    named.criterion
                )
            })
    }

    /// The task branch's head commit and that commit's tree, read host-side.
    async fn head(&self) -> Result<(String, String), String> {
        let repo_root = self.worktree.repo_root.clone();
        let branch = self.worktree.branch.clone();
        tokio::task::spawn_blocking(move || octos_core::branch_commit_and_tree(&repo_root, &branch))
            .await
            .map_err(|err| format!("reading the task branch failed: {err}"))?
            .map_err(|err| format!("reading the task branch failed: {err:#}"))
    }

    fn record_error(&self, op: &str, err: &eyre::Report) -> Stop {
        tracing::error!(
            fleet_id = %self.fleet_id, task_id = %self.task_id, attempt_id = %self.attempt_id,
            error = %err,
            "fleet worker: {op} errored",
        );
        Stop(AttemptOutcome::RecordError {
            reason: format!("{op} errored: {err}"),
        })
    }
}

/// Whether a committed run exited 0.
fn receipt_passed(receipt: &VerificationRecord) -> bool {
    receipt.result.as_ref().and_then(|r| r.exit_status) == Some(0)
}

/// The evidence an accepted verdict carries for a passing run.
fn receipt_evidence(receipt: &VerificationRecord) -> EvidenceRef {
    EvidenceRef {
        kind: "test_receipt".to_string(),
        locator: format!("verification:{}", receipt.op_id),
        sha256: receipt
            .result
            .as_ref()
            .map(|r| r.output_sha256.clone())
            .unwrap_or_default(),
        captured_at_ms: receipt.result.as_ref().map_or(0, |r| r.finished_at_ms),
    }
}

/// Why a committed run that did not pass rejects the attempt.
fn failure(request: &VerificationRequest, receipt: &VerificationRecord) -> String {
    let status = receipt
        .result
        .as_ref()
        .and_then(|r| r.exit_status)
        .map_or_else(|| "a signal".to_string(), |code| format!("exit {code}"));
    format!(
        "acceptance failed — {}: `{}` ended with {status} in a clean checkout of commit {}",
        request.criterion_id, request.command, request.commit
    )
}

fn rejected(reason: String) -> (AcceptanceVerdict, Option<String>) {
    (
        AcceptanceVerdict::Rejected {
            reason: reason.clone(),
        },
        Some(reason),
    )
}

fn terminated(reason: String) -> (AcceptanceVerdict, Option<String>) {
    (
        AcceptanceVerdict::Terminated {
            reason: reason.clone(),
        },
        Some(reason),
    )
}

/// What an acceptance command produced.
struct CommandOutput {
    /// The exit code; `None` when a signal ended the process.
    exit_status: Option<i32>,
    /// See [`output_digest`].
    sha256: String,
}

enum CommandFailure {
    /// The command outlived its budget; it and its process group were killed.
    TimedOut,
    /// The command was refused or could not be spawned or awaited.
    CannotRun(String),
}

/// Run an acceptance command inside the worker's sandbox and capture its exit
/// status and output digest.
///
/// The contract matches the ordinary `CommandExit` gate: `cmd` is split on
/// whitespace into argv (no shell syntax is honored — each token is quoted
/// before the sandbox's `sh -c` sees it), the safety policy is consulted first,
/// controller secrets are stripped from the environment, and the process runs
/// in its own group so a timeout kills the whole tree.
async fn run_command(
    sandbox: &Arc<dyn Sandbox>,
    cwd: &Path,
    cmd: &str,
    budget: Duration,
) -> Result<CommandOutput, CommandFailure> {
    if matches!(
        SafePolicy::default().check(cmd, cwd),
        Decision::Deny | Decision::Ask
    ) {
        return Err(CommandFailure::CannotRun(
            "denied by the command safety policy".to_string(),
        ));
    }
    let quoted = cmd
        .split_whitespace()
        .map(posix_quote)
        .collect::<Vec<_>>()
        .join(" ");
    if quoted.is_empty() {
        return Err(CommandFailure::CannotRun(
            "the command is empty".to_string(),
        ));
    }
    if budget.is_zero() {
        return Err(CommandFailure::TimedOut);
    }

    let mut command = sandboxed_git_command(sandbox, cwd, &quoted);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let child = command
        .spawn()
        .map_err(|err| CommandFailure::CannotRun(format!("failed to spawn: {err}")))?;
    let pid = child.id();
    match tokio::time::timeout(budget, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(CommandOutput {
            exit_status: output.status.code(),
            sha256: output_digest(&output.stdout, &output.stderr),
        }),
        Ok(Err(err)) => Err(CommandFailure::CannotRun(format!("wait failed: {err}"))),
        Err(_elapsed) => {
            if let Some(pid) = pid {
                octos_agent::kill_child_process(pid).await;
            }
            Err(CommandFailure::TimedOut)
        }
    }
}

/// SHA-256 over a command's captured output, framed so the stdout/stderr
/// boundary is unambiguous: the stdout length (u64, little-endian), then
/// stdout, then stderr. Lower-case hex.
fn output_digest(stdout: &[u8], stderr: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((stdout.len() as u64).to_le_bytes());
    hasher.update(stdout);
    hasher.update(stderr);
    format!("{:x}", hasher.finalize())
}

/// Single-quote `token` for a POSIX `sh -c` command line, so no character in
/// it is interpreted by the shell.
fn posix_quote(token: &str) -> String {
    let mut out = String::with_capacity(token.len() + 2);
    out.push('\'');
    for ch in token.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_frame_output_digest_so_the_stream_boundary_matters() {
        // The same bytes split differently between the streams must not
        // collide, and the digest is plain lower-case hex of SHA-256.
        let a = output_digest(b"ab", b"c");
        let b = output_digest(b"a", b"bc");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(a, output_digest(b"ab", b"c"), "deterministic");
    }

    #[test]
    fn should_quote_every_token_so_the_shell_interprets_nothing() {
        assert_eq!(posix_quote("plain"), "'plain'");
        assert_eq!(posix_quote("$(id)"), "'$(id)'");
        assert_eq!(posix_quote("it's"), "'it'\\''s'");
        assert_eq!(posix_quote("a;b"), "'a;b'");
    }
}
