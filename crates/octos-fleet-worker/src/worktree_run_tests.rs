//! Kill-point tests for worktree attempts, driven through the real pool,
//! a real git repository and a real (test-double) sandbox.
//!
//! "The host dies" is modelled exactly: the attempt stops at a chosen point and
//! never runs another line (a model call that never answers, or an armed
//! [`KillPoint`]). Nothing is cleaned up — no drop-guard fires — so the store
//! and the repository hold precisely what a killed process would leave. The
//! "next boot" then reconciles under a new owner epoch and dispatches again.
//!
//! The admitted test is `sh check.sh`: it appends a line to a counter file
//! OUTSIDE every tree (so each real run is observable) and passes iff
//! `value.txt` contains `fixed`. It runs in a clean checkout of the commit
//! under test, never in the task's own checkout.
//!
//! POSIX-sh contract: the worktree flow's populate/commit commands are POSIX sh
//! strings, so the whole module is Unix-only (like the pool's worktree tests).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use octos_fleet::{
    AcceptanceCriterion, AcceptanceVerdict, AttemptStatus, ChildStatus, FailingTest,
    FleetChildRecord, FleetKernelStore, LaunchOutcome, TaskSpec, VerificationPhase,
    VerificationRecord, Verifier, WorktreeBindRequest,
};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use tempfile::TempDir;
use tokio::sync::oneshot;

use crate::testutil::*;
use crate::worktree_run::{KillPoint, kill_points};
use crate::{AttemptOutcome, FleetWorkerPool, PoolConfig};

const FLEET: &str = "f1";
const TASK: &str = "a";
const BRANCH: &str = "fleet/f1/a";
const TEST_COMMAND: &str = "sh check.sh";

/// How long a test waits for an attempt to reach a point before failing.
const REACH: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A fleet with one worktree task on a real git repository whose base revision
/// holds `value.txt` and the counting test script. Unless built with
/// [`Fixture::with_task`], the task names `sh check.sh` as its failing test.
struct Fixture {
    /// Removes the fixture's directories when it goes away. `None` when the
    /// fixture was opened under a root somebody else owns.
    _root: Option<TempDir>,
    store: Arc<FleetKernelStore>,
    repo: Dir,
    work: Dir,
    counter: Dir,
    base_commit: String,
    base_tree: String,
}

/// One directory of the fixture. They all live under a single root, so a
/// second PROCESS can be pointed at the fixture the first one built.
struct Dir(PathBuf);

impl Dir {
    fn path(&self) -> &Path {
        &self.0
    }
}

fn failing_test_task(command: &str) -> TaskSpec {
    TaskSpec {
        failing_test: Some(FailingTest {
            criterion: "test".to_string(),
        }),
        ..task_spec_granted(
            TASK,
            &[],
            vec![AcceptanceCriterion {
                id: "test".to_string(),
                description: "the named test passes".to_string(),
                verifier: Verifier::CommandExit {
                    cmd: command.to_string(),
                    code: 0,
                },
            }],
            host_grant(),
        )
    }
}

impl Fixture {
    /// `value` is `value.txt` at the base revision; `None` when git is missing.
    async fn new(value: &str) -> Option<Self> {
        Self::with(value, TEST_COMMAND, "grep -q fixed value.txt").await
    }

    /// `command` is the admitted test; `check_body` is what `check.sh` does
    /// after counting the run.
    async fn with(value: &str, command: &str, check_body: &str) -> Option<Self> {
        Self::with_task(value, check_body, failing_test_task(command)).await
    }

    /// A fixture whose one task is `task` (its id must be [`TASK`]). In
    /// `check_body`, `@FLEET_DIR@` stands for the directory that holds the
    /// fleet's task checkouts.
    async fn with_task(value: &str, check_body: &str, task: TaskSpec) -> Option<Self> {
        let root = TempDir::new().unwrap();
        let fixture = Self::create_under(root.path(), value, check_body, task).await?;
        Some(Self {
            _root: Some(root),
            ..fixture
        })
    }

    /// Build the fixture under `root`: the store, the repository with its base
    /// revision, and the fleet. `None` when git is missing.
    async fn create_under(
        root: &Path,
        value: &str,
        check_body: &str,
        task: TaskSpec,
    ) -> Option<Self> {
        let (repo, work, counter) = Self::dirs(root);
        for dir in [&repo, &work, &counter] {
            std::fs::create_dir_all(dir.path()).unwrap();
        }
        if !git_init_repo(repo.path()) {
            return None;
        }
        let store = Arc::new(FleetKernelStore::open(root.join("store")).await.unwrap());
        let check_body = check_body.replace(
            "@FLEET_DIR@",
            &work.path().join(FLEET).display().to_string(),
        );
        std::fs::write(repo.path().join("value.txt"), value).unwrap();
        std::fs::write(
            repo.path().join("check.sh"),
            format!(
                "echo run >> '{}'\n{check_body}\n",
                counter.path().join("runs").display()
            ),
        )
        .unwrap();
        let fixture = Self {
            _root: None,
            store,
            repo,
            work,
            counter,
            base_commit: String::new(),
            base_tree: String::new(),
        };
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-q", "-m", "base with a failing test"]);
        let base_commit = fixture.git(&["rev-parse", "HEAD"]);
        let base_tree = fixture.git(&["rev-parse", "HEAD^{tree}"]);
        create_fleet_with_root(
            fixture.store.clone(),
            FLEET,
            Some(fixture.repo.path().to_string_lossy().into_owned()),
            vec![task],
        )
        .await;
        Some(Self {
            base_commit,
            base_tree,
            ..fixture
        })
    }

    /// Open the fixture ANOTHER process built under `root`, exactly as that
    /// process left it: the store is opened, nothing is created or reset.
    async fn reopen_under(root: &Path) -> Self {
        let (repo, work, counter) = Self::dirs(root);
        let store = Arc::new(FleetKernelStore::open(root.join("store")).await.unwrap());
        let fixture = Self {
            _root: None,
            store,
            repo,
            work,
            counter,
            base_commit: String::new(),
            base_tree: String::new(),
        };
        // The user's checkout never leaves the base revision: all of the
        // fleet's work happens on the task branch, in worktrees.
        let base_commit = fixture.git(&["rev-parse", "HEAD"]);
        let base_tree = fixture.git(&["rev-parse", "HEAD^{tree}"]);
        Self {
            base_commit,
            base_tree,
            ..fixture
        }
    }

    fn dirs(root: &Path) -> (Dir, Dir, Dir) {
        (
            Dir(root.join("repo")),
            Dir(root.join("work")),
            Dir(root.join("counter")),
        )
    }

    /// Run `git <args>` in the controller repository; trimmed stdout.
    fn git(&self, args: &[&str]) -> String {
        git_in(self.repo.path(), args)
    }

    /// `<work>/<fleet>/<task>`: the name every directory of the task starts with.
    fn stem(&self) -> PathBuf {
        self.work.path().join(FLEET).join(TASK)
    }

    /// The names of the task's directories that are on disk right now whose
    /// suffix starts with `kind` (`work-` or `verify-`).
    fn task_dirs(&self, kind: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.work.path().join(FLEET)) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&format!("{TASK}.{kind}")))
            .collect()
    }

    /// The directory recorded as holding the task's work.
    async fn bound_checkout(&self) -> PathBuf {
        PathBuf::from(
            self.child()
                .await
                .worktree
                .expect("the worktree is bound")
                .checkout,
        )
    }

    /// The clean checkouts of this task that are on disk right now.
    fn clean_checkouts(&self) -> Vec<String> {
        self.task_dirs("verify-")
    }

    /// How many times the admitted test actually ran.
    fn runs(&self) -> usize {
        std::fs::read_to_string(self.counter.path().join("runs"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// A pool for one "boot" of the host, identified by its lease `epoch`.
    async fn boot(&self, provider: Arc<dyn LlmProvider>, epoch: u64) -> FleetWorkerPool {
        let (_memory_dir, factory) = factory_for(provider).await;
        self.boot_with(factory, epoch)
    }

    /// [`Fixture::boot`] with the agent factory (and so the sandbox) given.
    fn boot_with(&self, factory: crate::AgentFactory, epoch: u64) -> FleetWorkerPool {
        FleetWorkerPool::new(
            self.store.clone(),
            Arc::new(factory),
            PoolConfig {
                global_concurrency: 4,
                per_fleet_concurrency: 4,
                deadline: Duration::from_secs(30),
                owner_epoch: epoch,
                lease_ttl_ms: TTL,
                projected_tokens: PROJECTED,
                workspace_root: self.work.path().to_path_buf(),
                keeper_profile_id: "test-keeper".to_owned(),
                repo_git_write_supported: true,
            },
            Arc::new(|| NOW),
        )
    }

    /// The host comes back as `epoch`: boot reconciliation interrupts the dead
    /// attempt and returns the task to `Ready`.
    async fn reconcile(&self, epoch: u64) {
        let report = self.store.reconcile(NOW + 1, epoch).await.unwrap();
        assert_eq!(
            report.interrupted.len(),
            1,
            "boot reconciliation must interrupt the dead attempt",
        );
        assert_eq!(self.child().await.status, ChildStatus::Ready);
    }

    async fn child(&self) -> FleetChildRecord {
        self.store.get_child(FLEET, TASK).await.unwrap().unwrap()
    }

    /// Dispatch the task and return its attempt id; the run continues detached.
    async fn start(&self, pool: &FleetWorkerPool) -> String {
        let dispatched = pool.dispatch(FLEET, TASK).await.unwrap();
        let LaunchOutcome::Launched { attempt_id } = dispatched.launch else {
            panic!("expected Launched, got {:?}", dispatched.launch);
        };
        attempt_id
    }

    /// Dispatch the task and wait for the attempt's outcome.
    async fn run(&self, pool: &FleetWorkerPool) -> (String, AttemptOutcome) {
        let dispatched = pool.dispatch(FLEET, TASK).await.unwrap();
        let LaunchOutcome::Launched { attempt_id } = dispatched.launch.clone() else {
            panic!("expected Launched, got {:?}", dispatched.launch);
        };
        let outcome = tokio::time::timeout(REACH, dispatched.handle.unwrap())
            .await
            .expect("the attempt must finish")
            .unwrap();
        (attempt_id, outcome)
    }

    /// The journal entries of `phase`, oldest first.
    async fn journal(&self, phase: VerificationPhase) -> Vec<VerificationRecord> {
        self.child()
            .await
            .verifications
            .into_iter()
            .filter(|r| r.phase == phase)
            .collect()
    }
}

/// Run `git <args>` in `dir`; trimmed stdout.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

async fn reached(point: oneshot::Receiver<()>, what: &str) {
    tokio::time::timeout(REACH, point)
        .await
        .unwrap_or_else(|_| panic!("the attempt never reached: {what}"))
        .unwrap();
}

/// The journal `op_id`s an accepted verdict's test receipts point at.
fn receipt_ops(outcome: &AttemptOutcome) -> Vec<String> {
    let AttemptOutcome::Completed {
        verdict: AcceptanceVerdict::Accepted { evidence },
    } = outcome
    else {
        panic!("expected an accepted completion, got {outcome:?}");
    };
    evidence
        .iter()
        .filter(|e| e.kind == "test_receipt")
        .map(|e| {
            e.locator
                .strip_prefix("verification:")
                .expect("a test receipt points at a journal entry")
                .to_string()
        })
        .collect()
}

fn rejection(outcome: &AttemptOutcome) -> &str {
    match outcome {
        AttemptOutcome::Completed {
            verdict: AcceptanceVerdict::Rejected { reason },
        } => reason,
        other => panic!("expected a rejected completion, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Scripted models
// ---------------------------------------------------------------------------

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 7,
        output_tokens: 3,
        ..Default::default()
    }
}

/// A scripted model. On its first call it writes `content` to `path` through
/// the worker's `write_file` tool. On the next call it either ends the turn or,
/// when built with [`PatchProvider::then_never_answers`], signals and never
/// answers — the model call that is outstanding when the host dies.
struct PatchProvider {
    path: String,
    content: String,
    calls: Arc<AtomicUsize>,
    /// What the model was shown on its first call (the task brief).
    brief: Arc<Mutex<String>>,
    outstanding: Mutex<Option<oneshot::Sender<()>>>,
    never_answers: bool,
}

impl PatchProvider {
    fn new(path: &str, content: &str) -> Self {
        Self {
            path: path.to_string(),
            content: content.to_string(),
            calls: Arc::new(AtomicUsize::new(0)),
            brief: Arc::new(Mutex::new(String::new())),
            outstanding: Mutex::new(None),
            never_answers: false,
        }
    }

    /// After writing the patch the model call stays outstanding forever; the
    /// receiver resolves once that call has been made (the patch is on disk).
    fn then_never_answers(path: &str, content: &str) -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let provider = Self {
            outstanding: Mutex::new(Some(tx)),
            never_answers: true,
            ..Self::new(path, content)
        };
        (provider, rx)
    }
}

#[async_trait]
impl LlmProvider for PatchProvider {
    async fn chat(&self, m: &[Message], _t: &[ToolSpec], _c: &ChatConfig) -> Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            *self.brief.lock().unwrap() = m
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![octos_core::ToolCall {
                    id: "call_patch".to_string(),
                    name: "write_file".to_string(),
                    arguments: serde_json::json!({
                        "path": self.path,
                        "content": self.content,
                    }),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: usage(),
                provider_index: None,
            });
        }
        if self.never_answers {
            if let Some(outstanding) = self.outstanding.lock().unwrap().take() {
                let _ = outstanding.send(());
            }
            std::future::pending::<()>().await;
        }
        Ok(ChatResponse {
            content: Some("patched".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: usage(),
            provider_index: None,
        })
    }
    fn model_id(&self) -> &str {
        "mock"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// A model that must never be called: it counts every call it receives.
fn uncalled_model() -> (Arc<dyn LlmProvider>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(CountingProvider {
            calls: calls.clone(),
        }),
        calls,
    )
}

// ---------------------------------------------------------------------------
// The uninterrupted path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_deliver_patch_with_tree_bound_receipt_when_test_fails_on_base() {
    let Some(fx) = Fixture::new("broken\n").await else {
        return; // git unavailable
    };
    let model = PatchProvider::new("value.txt", "fixed\n");
    let pool = fx.boot(Arc::new(model), EPOCH).await;

    let (attempt, outcome) = fx.run(&pool).await;

    // The receipt in the verdict is the journal entry of the acceptance run.
    let ops = receipt_ops(&outcome);
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(acceptance.len(), 1, "one acceptance run");
    assert_eq!(ops, vec![acceptance[0].op_id.clone()]);

    // It names the command, the exact tree, the exit status and an output digest.
    let receipt = &acceptance[0];
    let head = fx.git(&["rev-parse", BRANCH]);
    assert_eq!(receipt.command, TEST_COMMAND);
    assert_eq!(receipt.attempt_id, attempt);
    assert_eq!(
        receipt.commit, head,
        "the receipt names the delivered commit"
    );
    assert_eq!(
        receipt.tree_digest,
        fx.git(&["rev-parse", &format!("{BRANCH}^{{tree}}")]),
        "the receipt is bound to the delivered tree",
    );
    assert_ne!(receipt.tree_digest, fx.base_tree);
    let result = receipt.result.as_ref().expect("a committed result");
    assert_eq!(result.exit_status, Some(0));
    assert_eq!(result.output_sha256.len(), 64);

    // Admission was recorded first: the same command FAILED on the base tree.
    let admission = fx.journal(VerificationPhase::Admission).await;
    assert_eq!(admission.len(), 1, "one admission run");
    assert_eq!(admission[0].commit, fx.base_commit);
    assert_eq!(admission[0].tree_digest, fx.base_tree);
    assert_eq!(
        admission[0].result.as_ref().unwrap().exit_status,
        Some(1),
        "the admitted test must fail on the base revision",
    );

    // The base was bound before any edit and the patch is exactly one commit
    // on top of it.
    let child = fx.child().await;
    let binding = child.worktree.as_ref().expect("the worktree is bound");
    assert_eq!(binding.base_commit, fx.base_commit);
    assert_eq!(binding.base_tree, fx.base_tree);
    assert_eq!(binding.branch, BRANCH);
    assert_eq!(child.status, ChildStatus::Succeeded);
    assert_eq!(
        fx.git(&[
            "rev-list",
            "--count",
            &format!("{}..{BRANCH}", fx.base_commit)
        ]),
        "1",
    );
    assert_eq!(
        fx.git(&["diff", "--name-only", &fx.base_commit, BRANCH]),
        "value.txt",
    );
    assert_eq!(
        fx.runs(),
        2,
        "the test ran once on the base and once on the patch"
    );

    // The branch is the deliverable; the accepted task's checkout is gone, and
    // so is every clean checkout its checks ran in.
    assert_eq!(fx.task_dirs(""), Vec::<String>::new());
    assert_eq!(
        fx.git(&["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1,
        "only the user's own checkout stays registered",
    );
}

#[tokio::test]
async fn should_refuse_admission_without_a_model_call_when_test_already_passes_on_base() {
    let Some(fx) = Fixture::new("fixed\n").await else {
        return;
    };
    let (model, calls) = uncalled_model();
    let pool = fx.boot(model, EPOCH).await;

    let (_, outcome) = fx.run(&pool).await;

    let reason = rejection(&outcome);
    assert!(
        reason.contains("already passes on base revision") && reason.contains(&fx.base_commit),
        "got: {reason}",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    assert_eq!(fx.child().await.status, ChildStatus::Failed);
    assert_eq!(
        fx.git(&["rev-parse", BRANCH]),
        fx.base_commit,
        "nothing was edited"
    );
    let admission = fx.journal(VerificationPhase::Admission).await;
    assert_eq!(
        admission[0].result.as_ref().unwrap().exit_status,
        Some(0),
        "the refusal is backed by a recorded run",
    );
}

#[tokio::test]
async fn should_refuse_admission_when_test_command_cannot_execute() {
    let Some(fx) = Fixture::with("broken\n", "no-such-test-runner-xyz --run", "true").await else {
        return;
    };
    let (model, calls) = uncalled_model();
    let pool = fx.boot(model, EPOCH).await;

    let (_, outcome) = fx.run(&pool).await;

    let reason = rejection(&outcome);
    assert!(reason.contains("cannot be executed"), "got: {reason}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a command that cannot run is not a failing test; no model call",
    );
}

#[tokio::test]
async fn should_refuse_dispatch_when_task_naming_a_failing_test_has_no_git_worktree() {
    // The controller workspace is not a git repository (no root at all): there
    // is no base revision to admit against, so nothing may be launched.
    let (_store_dir, store) = fresh_store().await;
    create_fleet(store.clone(), FLEET, vec![failing_test_task(TEST_COMMAND)]).await;
    let work = TempDir::new().unwrap();
    let (model, calls) = uncalled_model();
    let (_memory_dir, factory) = factory_for(model).await;
    let pool = FleetWorkerPool::new(
        store.clone(),
        Arc::new(factory),
        PoolConfig {
            global_concurrency: 1,
            per_fleet_concurrency: 1,
            deadline: Duration::from_secs(30),
            owner_epoch: EPOCH,
            lease_ttl_ms: TTL,
            projected_tokens: PROJECTED,
            workspace_root: work.path().to_path_buf(),
            keeper_profile_id: "test-keeper".to_owned(),
            repo_git_write_supported: true,
        },
        Arc::new(|| NOW),
    );

    let err = pool.dispatch(FLEET, TASK).await.unwrap_err();

    assert!(
        err.to_string().contains("isolated git worktree"),
        "got: {err}"
    );
    let child = store.get_child(FLEET, TASK).await.unwrap().unwrap();
    assert_eq!(child.status, ChildStatus::Ready, "no durable attempt");
    assert_eq!(child.attempts_used, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Kill the host after the patch is written, before verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_recover_uncommitted_patch_with_zero_model_calls_when_host_dies_before_verification()
{
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };

    // Boot 1: the model writes the fix, then its next call never returns.
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    let dead_attempt = fx.start(&boot1).await;
    reached(
        patch_written,
        "the patch on disk with a model call outstanding",
    )
    .await;

    // What the dead host left: the patch is in the checkout and NOT committed.
    let dead_checkout = fx.bound_checkout().await;
    assert_eq!(
        std::fs::read_to_string(dead_checkout.join("value.txt")).unwrap(),
        "fixed\n",
    );
    assert_eq!(fx.git(&["rev-parse", BRANCH]), fx.base_commit);
    assert_eq!(fx.child().await.status, ChildStatus::Running);
    assert_eq!(fx.runs(), 1, "only the admission run happened");

    // Meanwhile the user keeps working in their own checkout.
    std::fs::write(fx.repo.path().join("seed.txt"), "the user's edit\n").unwrap();
    std::fs::write(fx.repo.path().join("notes.txt"), "the user's notes\n").unwrap();
    let user_head = fx.git(&["rev-parse", "HEAD"]);

    // Boot 2: reconcile, dispatch — with a model that must not be called.
    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (attempt, outcome) = fx.run(&boot2).await;

    // The patch was recovered in the bound worktree, verified, and delivered.
    let ops = receipt_ops(&outcome);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "recovery made zero model calls"
    );
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(acceptance.len(), 1);
    let receipt = &acceptance[0];
    assert_eq!(ops, vec![receipt.op_id.clone()]);
    assert_eq!(receipt.attempt_id, attempt);
    assert_ne!(receipt.attempt_id, dead_attempt);
    assert_eq!(receipt.commit, fx.git(&["rev-parse", BRANCH]));
    assert_eq!(
        receipt.tree_digest,
        fx.git(&["rev-parse", &format!("{BRANCH}^{{tree}}")]),
        "the receipt is for the actual recovered tree",
    );
    assert_eq!(receipt.result.as_ref().unwrap().exit_status, Some(0));
    assert_eq!(
        fx.git(&["show", &format!("{BRANCH}:value.txt")]),
        "fixed",
        "the recovered patch is what was delivered",
    );
    assert_eq!(
        fx.git(&[
            "rev-list",
            "--count",
            &format!("{}..{BRANCH}", fx.base_commit)
        ]),
        "1",
        "the edit was applied once",
    );

    // The base did not move, admission was not repeated, and nothing was spent.
    let child = fx.child().await;
    assert_eq!(child.status, ChildStatus::Succeeded);
    assert_eq!(
        child.worktree.as_ref().unwrap().base_commit,
        fx.base_commit,
        "the base revision is immutable across attempts",
    );
    assert_eq!(fx.journal(VerificationPhase::Admission).await.len(), 1);
    assert_eq!(
        fx.runs(),
        2,
        "one admission run, one run on the recovered tree"
    );
    assert_eq!(child.tokens_committed, 0);
    let dead = fx
        .store
        .get_attempt(TASK, &dead_attempt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dead.status, AttemptStatus::Interrupted);
    assert!(
        !dead_checkout.exists(),
        "the dead attempt's directory was retired once its work was committed",
    );

    // The user's independent edits survived, untouched.
    assert_eq!(
        std::fs::read_to_string(fx.repo.path().join("seed.txt")).unwrap(),
        "the user's edit\n",
    );
    assert_eq!(
        std::fs::read_to_string(fx.repo.path().join("notes.txt")).unwrap(),
        "the user's notes\n",
    );
    assert_eq!(fx.git(&["rev-parse", "HEAD"]), user_head);
}

#[tokio::test]
async fn should_continue_from_recovered_tree_when_recovered_work_does_not_pass_yet() {
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };

    // Boot 1 dies with half a patch on disk.
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "halfway\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(patch_written, "the partial patch on disk").await;

    // Boot 2: the recovered tree fails the test, so the model IS called — and
    // told it is resuming, from which base, with the recovered work in place.
    fx.reconcile(EPOCH + 1).await;
    let model = PatchProvider::new("value.txt", "fixed\n");
    let (calls, brief) = (model.calls.clone(), model.brief.clone());
    let boot2 = fx.boot(Arc::new(model), EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot2).await;

    receipt_ops(&outcome);
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the model finishes the work"
    );
    let brief = brief.lock().unwrap().clone();
    assert!(brief.contains("Resuming interrupted work"), "got: {brief}");
    assert!(
        brief.contains(&fx.base_commit),
        "the brief names the base revision: {brief}",
    );

    // The interrupted attempt's work is kept as its own commit, under the fix.
    assert_eq!(
        fx.git(&[
            "rev-list",
            "--count",
            &format!("{}..{BRANCH}", fx.base_commit)
        ]),
        "2",
        "the recovered snapshot and the finishing commit",
    );
    assert_eq!(
        fx.git(&["show", &format!("{BRANCH}~1:value.txt")]),
        "halfway"
    );
    assert_eq!(fx.git(&["show", &format!("{BRANCH}:value.txt")]), "fixed");

    // Admission ran once; the recovered tree was checked before the model was
    // called, and the final tree after.
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(acceptance.len(), 2);
    assert_eq!(
        acceptance[0].result.as_ref().unwrap().exit_status,
        Some(1),
        "the recovered tree did not pass",
    );
    assert_eq!(acceptance[1].result.as_ref().unwrap().exit_status, Some(0));
    assert_eq!(fx.runs(), 3);
}

#[tokio::test]
async fn should_restore_from_branch_when_dead_attempt_never_populated_its_checkout() {
    // The host died after the worktree was bound but before the worker
    // finished populating it. That checkout is EMPTY: committing it would
    // delete every file. It must be rebuilt from the branch, never snapshotted.
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let dead_attempt = launch(&fx.store, FLEET, TASK).await;
    fx.store.mark_running(TASK, &dead_attempt).await.unwrap();
    let empty = octos_core::new_work_checkout(&fx.stem()).unwrap();
    octos_core::prepare_fleet_worktree(fx.repo.path(), fx.work.path(), BRANCH, &empty)
        .expect("the controller creates the (empty) checkout");
    fx.store
        .bind_worktree(
            FLEET,
            TASK,
            &dead_attempt,
            EPOCH,
            WorktreeBindRequest {
                repo_root: fx.repo.path().display().to_string(),
                branch: BRANCH.to_string(),
                checkout: empty.display().to_string(),
                base_commit: fx.base_commit.clone(),
                base_tree: fx.base_tree.clone(),
            },
            NOW,
        )
        .await
        .unwrap();
    assert!(!empty.join("value.txt").exists(), "never populated");

    fx.reconcile(EPOCH + 1).await;
    let boot2 = fx
        .boot(
            Arc::new(PatchProvider::new("value.txt", "fixed\n")),
            EPOCH + 1,
        )
        .await;
    let (_, outcome) = fx.run(&boot2).await;

    receipt_ops(&outcome);
    assert_eq!(
        fx.git(&["diff", "--name-only", &fx.base_commit, BRANCH]),
        "value.txt",
        "only the fix changed; no file was deleted by snapshotting an empty checkout",
    );
    assert_eq!(fx.git(&["show", &format!("{BRANCH}:seed.txt")]), "seed");
}

#[tokio::test]
async fn should_keep_recovered_checkout_when_relaunch_is_rejected() {
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(patch_written, "the patch on disk").await;
    let dead_checkout = fx.bound_checkout().await;
    fx.reconcile(EPOCH + 1).await;

    // The goal is cleared while the host is down: the next dispatch is rejected.
    assert!(fx.store.cancel_fleet(FLEET, NOW + 2).await.unwrap());
    let (model, _) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let dispatched = boot2.dispatch(FLEET, TASK).await.unwrap();

    assert_eq!(dispatched.launch, LaunchOutcome::RejectedFleetTerminal);
    assert_eq!(
        std::fs::read_to_string(dead_checkout.join("value.txt")).unwrap(),
        "fixed\n",
        "a rejected dispatch must not delete the work it found",
    );
    assert_eq!(
        fx.task_dirs("work-").len(),
        1,
        "and the empty checkout it had created for itself is rolled back",
    );
}

/// Boot 1 writes the fix and dies with it uncommitted, after `left` was run in
/// its checkout — what the dead attempt did to its own `HEAD` — and an
/// untracked file was added. Boot 2 must deliver all of it without a model.
async fn recovers_work_left_off_the_task_branch(left: &[&str]) {
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(patch_written, "the patch on disk").await;
    let dead_checkout = fx.bound_checkout().await;
    git_in(&dead_checkout, left);
    std::fs::write(dead_checkout.join("notes.txt"), "untracked\n").unwrap();

    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot2).await;

    receipt_ops(&outcome);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    assert_eq!(fx.git(&["show", &format!("{BRANCH}:value.txt")]), "fixed");
    assert_eq!(
        fx.git(&["show", &format!("{BRANCH}:notes.txt")]),
        "untracked",
        "the untracked file was recovered too",
    );
    assert!(
        !dead_checkout.exists(),
        "retired once its work was committed"
    );
}

#[tokio::test]
async fn should_recover_work_when_the_dead_attempt_left_its_head_detached() {
    recovers_work_left_off_the_task_branch(&["checkout", "-q", "--detach"]).await;
}

#[tokio::test]
async fn should_recover_work_when_the_dead_attempt_left_its_head_on_another_branch() {
    recovers_work_left_off_the_task_branch(&["checkout", "-q", "-b", "elsewhere"]).await;
}

#[tokio::test]
async fn should_refuse_dispatch_and_keep_the_directory_when_its_work_cannot_be_committed() {
    // The dead attempt's directory lost its `.git` file, so git cannot commit
    // from it — and it holds the only copy of the patch. It must not be swept
    // away as a leftover: the dispatch is refused until somebody has dealt
    // with it.
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(patch_written, "the patch on disk").await;
    let dead_checkout = fx.bound_checkout().await;
    std::fs::remove_file(dead_checkout.join(".git")).unwrap();

    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let refused = boot2
        .dispatch(FLEET, TASK)
        .await
        .expect_err("the dispatch is refused");

    let refused = format!("{refused:#}");
    assert!(
        refused.contains(&dead_checkout.display().to_string()),
        "the refusal names the directory: {refused}",
    );
    assert_eq!(
        std::fs::read_to_string(dead_checkout.join("value.txt")).unwrap(),
        "fixed\n",
        "the work is still where it was",
    );
    assert_eq!(
        fx.task_dirs(""),
        vec![
            dead_checkout
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        ],
        "nothing of the task was removed, and nothing was created",
    );
    assert_eq!(fx.child().await.status, ChildStatus::Ready);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no attempt ran");

    // Once it has been moved away, the task runs again from the branch.
    std::fs::rename(&dead_checkout, fx.work.path().join("saved")).unwrap();
    let model = Arc::new(PatchProvider::new("value.txt", "fixed\n"));
    let boot3 = fx.boot(model, EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot3).await;
    receipt_ops(&outcome);
}

#[tokio::test]
async fn should_deliver_recovered_work_when_host_dies_before_the_dead_directory_is_retired() {
    // Boot 2 commits what boot 1 left, records its own directory as the one
    // holding the work — and dies before it retires boot 1's directory. Boot 3
    // finds two directories: the recorded one is recovered from, the other is
    // a leftover. The work is delivered once, with no model call.
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(patch_written, "the patch on disk").await;
    let first = fx.bound_checkout().await;

    fx.reconcile(EPOCH + 1).await;
    let recorded = kill_points::arm(&fx.store, KillPoint::BeforeRetiringDead);
    let (model, _) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    fx.start(&boot2).await;
    reached(
        recorded,
        "the new directory recorded, the old one not retired",
    )
    .await;
    let second = fx.bound_checkout().await;
    assert_ne!(second, first, "the record names boot 2's directory");
    assert!(first.exists(), "boot 1's directory has not been retired");

    fx.reconcile(EPOCH + 2).await;
    let (model, calls) = uncalled_model();
    let boot3 = fx.boot(model, EPOCH + 2).await;
    let (_, outcome) = fx.run(&boot3).await;

    receipt_ops(&outcome);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    assert_eq!(fx.git(&["show", &format!("{BRANCH}:value.txt")]), "fixed");
    assert_eq!(
        fx.git(&[
            "rev-list",
            "--count",
            &format!("{}..{BRANCH}", fx.base_commit)
        ]),
        "1",
        "the recovered work was committed once",
    );
    assert!(!first.exists() && !second.exists(), "both were cleaned up");
}

// ---------------------------------------------------------------------------
// Kill the host after the test passes, before acknowledgement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_reuse_committed_receipt_without_rerunning_test_when_host_dies_before_completion() {
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };

    // Boot 1: the test passes and its result is committed; the host dies before
    // the task's completion is.
    let completion = kill_points::arm(&fx.store, KillPoint::BeforeCompletion);
    let boot1 = fx
        .boot(Arc::new(PatchProvider::new("value.txt", "fixed\n")), EPOCH)
        .await;
    let dead_attempt = fx.start(&boot1).await;
    reached(completion, "a computed verdict that is not committed").await;

    let committed = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].attempt_id, dead_attempt);
    assert_eq!(committed[0].result.as_ref().unwrap().exit_status, Some(0));
    assert_eq!(
        fx.child().await.status,
        ChildStatus::Running,
        "not acknowledged"
    );
    assert_eq!(fx.runs(), 2);

    // Boot 2 completes the task from the committed receipt.
    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot2).await;

    assert_eq!(
        receipt_ops(&outcome),
        vec![committed[0].op_id.clone()],
        "the delivered receipt IS the one committed before the host died",
    );
    assert_eq!(fx.runs(), 2, "the test was not run again");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    assert_eq!(
        fx.journal(VerificationPhase::Acceptance).await,
        committed,
        "no new journal entry",
    );
    assert_eq!(fx.child().await.status, ChildStatus::Succeeded);

    // Delivered once: the task is terminal, so a redelivered wake launches nothing.
    let again = boot2.dispatch(FLEET, TASK).await.unwrap();
    assert_eq!(again.launch, LaunchOutcome::RejectedNotReady);
    assert!(again.handle.is_none());
    assert_eq!(fx.runs(), 2);
}

#[tokio::test]
async fn should_rerun_only_the_test_on_the_same_tree_when_its_result_was_lost() {
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };

    // Boot 1: the test exits, and the host dies before its result is committed.
    let exited = kill_points::arm(&fx.store, KillPoint::AcceptanceCommandExited);
    let boot1 = fx
        .boot(Arc::new(PatchProvider::new("value.txt", "fixed\n")), EPOCH)
        .await;
    fx.start(&boot1).await;
    reached(exited, "a finished test whose result is not committed").await;

    let lost = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].result, None, "the outcome is unknown");
    assert_eq!(fx.runs(), 2);
    assert_eq!(
        fx.clean_checkouts().len(),
        1,
        "the dead host left the checkout its test was running in",
    );

    // Boot 2: the unknown result permits a rerun of the test — and only that.
    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (attempt, outcome) = fx.run(&boot2).await;

    let ops = receipt_ops(&outcome);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    assert_eq!(fx.runs(), 3, "exactly one rerun");
    let journal = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(journal.len(), 2);
    assert_eq!(
        journal[0], lost[0],
        "the lost run stays recorded as unknown"
    );
    let rerun = &journal[1];
    assert_eq!(ops, vec![rerun.op_id.clone()]);
    assert_eq!(rerun.attempt_id, attempt);
    assert_eq!(
        (rerun.commit.as_str(), rerun.tree_digest.as_str()),
        (lost[0].commit.as_str(), lost[0].tree_digest.as_str()),
        "the rerun is on the same commit and tree as the lost run",
    );
    assert_eq!(rerun.result.as_ref().unwrap().exit_status, Some(0));
    assert_eq!(
        fx.git(&[
            "rev-list",
            "--count",
            &format!("{}..{BRANCH}", fx.base_commit)
        ]),
        "1",
        "the patch was not applied a second time",
    );
    assert_eq!(
        fx.clean_checkouts(),
        Vec::<String>::new(),
        "the dead host's clean checkout was removed with the successor's own",
    );
}

#[tokio::test]
async fn should_run_every_acceptance_command_again_when_one_of_their_results_was_lost() {
    // Two acceptance commands; the second only passes after the first has run
    // in the same directory (the first prepares something git does not track).
    // The host dies with the first result committed and the second lost. The
    // successor's clean checkout has nothing the first command prepared, so
    // reusing its committed result and running only the second would fail a
    // commit that is fine. Both run again, in order.
    let task = TaskSpec {
        failing_test: Some(FailingTest {
            criterion: "test".to_string(),
        }),
        ..task_spec_granted(
            TASK,
            &[],
            vec![
                AcceptanceCriterion {
                    id: "setup".to_string(),
                    description: "the workspace is prepared".to_string(),
                    verifier: Verifier::CommandExit {
                        cmd: "sh setup.sh".to_string(),
                        code: 0,
                    },
                },
                AcceptanceCriterion {
                    id: "test".to_string(),
                    description: "the named test passes".to_string(),
                    verifier: Verifier::CommandExit {
                        cmd: TEST_COMMAND.to_string(),
                        code: 0,
                    },
                },
            ],
            host_grant(),
        )
    };
    let Some(fx) = Fixture::with_task(
        "broken\n",
        "test -f prepared.flag && grep -q fixed value.txt",
        task,
    )
    .await
    else {
        return;
    };
    std::fs::write(fx.repo.path().join("setup.sh"), "echo ok > prepared.flag\n").unwrap();
    std::fs::write(fx.repo.path().join(".git/info/exclude"), "prepared.flag\n").unwrap();
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "add the setup step"]);

    // Boot 1: `setup` passes and is committed; `test` exits and the host dies.
    let exited = kill_points::arm_after(&fx.store, KillPoint::AcceptanceCommandExited, 1);
    let boot1 = fx
        .boot(Arc::new(PatchProvider::new("value.txt", "fixed\n")), EPOCH)
        .await;
    fx.start(&boot1).await;
    reached(exited, "the second command finished with its result lost").await;
    let before = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(before.len(), 2);
    assert_eq!(before[0].criterion_id, "setup");
    assert_eq!(before[0].result.as_ref().unwrap().exit_status, Some(0));
    assert_eq!(before[1].criterion_id, "test");
    assert_eq!(before[1].result, None);

    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (attempt, outcome) = fx.run(&boot2).await;

    let ops = receipt_ops(&outcome);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    let journal = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(journal.len(), 4, "both commands were run again");
    assert_eq!(
        journal[2..]
            .iter()
            .map(|r| (r.criterion_id.as_str(), r.attempt_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("setup", attempt.as_str()), ("test", attempt.as_str())],
    );
    assert_eq!(
        ops,
        vec![journal[2].op_id.clone(), journal[3].op_id.clone()],
        "the verdict rests on the runs made together in one checkout",
    );
    assert!(
        journal[2..]
            .iter()
            .all(|r| r.commit == before[0].commit && r.tree_digest == before[0].tree_digest),
        "on the same commit as before",
    );
}

#[tokio::test]
async fn should_not_repeat_admission_when_host_dies_after_it_was_committed() {
    // Die with the admission result committed and no patch at all (the model
    // call that follows admission never returns, before any edit).
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let (model, outstanding) = PatchProvider::then_never_answers("unrelated.tmp", "x\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(outstanding, "a model call outstanding after admission").await;
    // Leave the tree exactly at the base, as if nothing had been written.
    std::fs::remove_file(fx.bound_checkout().await.join("unrelated.tmp")).unwrap();
    assert_eq!(fx.runs(), 1);

    fx.reconcile(EPOCH + 1).await;
    let boot2 = fx
        .boot(
            Arc::new(PatchProvider::new("value.txt", "fixed\n")),
            EPOCH + 1,
        )
        .await;
    let (_, outcome) = fx.run(&boot2).await;

    receipt_ops(&outcome);
    assert_eq!(
        fx.journal(VerificationPhase::Admission).await.len(),
        1,
        "the committed admission result was reused",
    );
    assert_eq!(fx.runs(), 2, "admission once, acceptance once");
}

// ---------------------------------------------------------------------------
// What the test writes, and what writes under the test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_deliver_only_the_committed_work_when_the_test_writes_files_of_its_own() {
    // The test generates a file every time it runs (think of a lockfile or a
    // snapshot). It runs in a throwaway checkout of the commit under test, so
    // what it writes is not work: the delivered commit is the agent's patch and
    // nothing else, and one run settles it.
    let Some(fx) = Fixture::with(
        "broken\n",
        TEST_COMMAND,
        "grep -q fixed value.txt || exit 1\nprintf 'generated\\n' > generated.lock",
    )
    .await
    else {
        return;
    };
    let pool = fx
        .boot(Arc::new(PatchProvider::new("value.txt", "fixed\n")), EPOCH)
        .await;

    let (_, outcome) = fx.run(&pool).await;

    let ops = receipt_ops(&outcome);
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(acceptance.len(), 1, "one run on the one candidate commit");
    assert_eq!(ops, vec![acceptance[0].op_id.clone()]);
    assert_eq!(
        acceptance[0].commit,
        fx.git(&["rev-parse", BRANCH]),
        "the receipt names the delivered commit",
    );
    assert_eq!(
        fx.git(&["diff", "--name-only", &fx.base_commit, BRANCH]),
        "value.txt",
        "the delivered commit holds the patch, not what the test wrote",
    );
}

#[tokio::test]
async fn should_reject_when_the_checkout_keeps_changing_while_it_is_verified() {
    // Something keeps writing into the task's own checkout while its commit is
    // being verified (here the check script does it, by absolute path, standing
    // in for any process still at work there). Every verified commit is stale
    // by the time it would be delivered, so nothing is.
    let Some(fx) = Fixture::with(
        "broken\n",
        TEST_COMMAND,
        "grep -q fixed value.txt || exit 1\n\
         for d in '@FLEET_DIR@'/a '@FLEET_DIR@'/a.work-*; do\n\
         if test -f \"$d/value.txt\"; then echo again >> \"$d/churn.log\"; fi\n\
         done",
    )
    .await
    else {
        return;
    };
    let pool = fx
        .boot(Arc::new(PatchProvider::new("value.txt", "fixed\n")), EPOCH)
        .await;

    let (_, outcome) = fx.run(&pool).await;

    let reason = rejection(&outcome);
    assert!(reason.contains("kept changing"), "got: {reason}");
    assert_eq!(fx.child().await.status, ChildStatus::Failed);
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(acceptance.len(), 2, "two different commits were verified");
    assert_ne!(acceptance[0].commit, acceptance[1].commit);
}

// ---------------------------------------------------------------------------
// The checkout may hold the only copy of the work
// ---------------------------------------------------------------------------

/// Wait until the task has settled as `Failed`, then give the pool's cleanup —
/// which runs right after the completion is recorded — time to finish, so that
/// "the checkout is still there" is asserted against a quiescent pool.
async fn settled_as_failed(fx: &Fixture) {
    tokio::time::timeout(REACH, async {
        while fx.child().await.status != ChildStatus::Failed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the cancelled attempt must be settled");
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn should_keep_uncommitted_work_when_attempt_is_cancelled_in_a_live_process() {
    // Not a host death: the process lives and the attempt's task goes away (a
    // runtime cancel, a panic, a store error). The pool's drop-guard settles
    // the attempt, and it must not delete the only copy of the patch with it.
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
    let pool = fx.boot(Arc::new(model), EPOCH).await;
    let dispatched = pool.dispatch(FLEET, TASK).await.unwrap();
    reached(
        patch_written,
        "the patch on disk with a model call outstanding",
    )
    .await;

    dispatched.handle.unwrap().abort();
    settled_as_failed(&fx).await;

    assert_eq!(
        std::fs::read_to_string(fx.bound_checkout().await.join("value.txt"))
            .ok()
            .as_deref(),
        Some("fixed\n"),
        "the uncommitted patch must survive the cancelled attempt",
    );
    assert_eq!(
        fx.git(&["rev-parse", BRANCH]),
        fx.base_commit,
        "nothing was committed, so the checkout is the only copy",
    );
}

#[tokio::test]
async fn should_keep_the_checkout_when_the_task_is_not_accepted() {
    // Only an accepted task's checkout is removed (its branch is the
    // deliverable). A task that did not pass keeps its checkout, so nothing
    // that was not delivered is deleted behind the user's back.
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let pool = fx
        .boot(
            Arc::new(PatchProvider::new("value.txt", "halfway\n")),
            EPOCH,
        )
        .await;

    let (_, outcome) = fx.run(&pool).await;

    rejection(&outcome);
    assert_eq!(
        std::fs::read_to_string(fx.bound_checkout().await.join("value.txt"))
            .ok()
            .as_deref(),
        Some("halfway\n"),
    );
}

// ---------------------------------------------------------------------------
// A worktree task that names no failing test
// ---------------------------------------------------------------------------

/// A worktree task with the full-trust grant, `acceptance` as given, and no
/// named failing test.
fn plain_task(acceptance: Vec<AcceptanceCriterion>) -> TaskSpec {
    task_spec_granted(TASK, &[], acceptance, host_grant())
}

fn command_criterion(command: &str) -> AcceptanceCriterion {
    AcceptanceCriterion {
        id: "check".to_string(),
        description: "the check passes".to_string(),
        verifier: Verifier::CommandExit {
            cmd: command.to_string(),
            code: 0,
        },
    }
}

#[tokio::test]
async fn should_resume_the_agent_from_recovered_work_when_task_names_no_failing_test() {
    // The task has NO acceptance at all, so nothing but its agent can say the
    // work is finished. The interrupted attempt never did: its leftover work is
    // kept and handed back to the agent, never delivered unseen and never reset.
    let Some(fx) = Fixture::with_task("broken\n", "true", plain_task(vec![])).await else {
        return;
    };
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "halfway\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(patch_written, "the partial work on disk").await;
    assert_eq!(fx.git(&["rev-parse", BRANCH]), fx.base_commit);

    fx.reconcile(EPOCH + 1).await;
    let model = PatchProvider::new("value.txt", "fixed\n");
    let (calls, brief) = (model.calls.clone(), model.brief.clone());
    let boot2 = fx.boot(Arc::new(model), EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot2).await;

    assert!(
        matches!(
            outcome,
            AttemptOutcome::Completed {
                verdict: AcceptanceVerdict::Accepted { .. }
            }
        ),
        "got {outcome:?}",
    );
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "recovered work is not delivered without the agent",
    );
    let brief = brief.lock().unwrap().clone();
    assert!(brief.contains("Resuming interrupted work"), "got: {brief}");
    assert_eq!(
        fx.git(&["show", &format!("{BRANCH}~1:value.txt")]),
        "halfway",
        "the interrupted attempt's work was recovered, not reset",
    );
    assert_eq!(fx.git(&["show", &format!("{BRANCH}:value.txt")]), "fixed");
    assert_eq!(
        fx.child().await.worktree.unwrap().base_commit,
        fx.base_commit,
        "the base revision is bound for every worktree task",
    );
}

#[tokio::test]
async fn should_record_tree_bound_receipts_when_task_names_no_failing_test() {
    // No admission run (there is no test that must fail on the base), but the
    // acceptance command still leaves a receipt bound to the delivered tree.
    let task = plain_task(vec![command_criterion(TEST_COMMAND)]);
    let Some(fx) = Fixture::with_task("broken\n", "grep -q fixed value.txt", task).await else {
        return;
    };
    let pool = fx
        .boot(Arc::new(PatchProvider::new("value.txt", "fixed\n")), EPOCH)
        .await;

    let (_, outcome) = fx.run(&pool).await;

    let ops = receipt_ops(&outcome);
    assert!(fx.journal(VerificationPhase::Admission).await.is_empty());
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(acceptance.len(), 1);
    assert_eq!(ops, vec![acceptance[0].op_id.clone()]);
    assert_eq!(
        acceptance[0].tree_digest,
        fx.git(&["rev-parse", &format!("{BRANCH}^{{tree}}")]),
    );
    assert_eq!(fx.runs(), 1, "the command ran once, on the patch");
}

fn file_criterion(path: &str) -> AcceptanceCriterion {
    AcceptanceCriterion {
        id: "file".to_string(),
        description: "the file is delivered".to_string(),
        verifier: Verifier::FileExists {
            path: path.to_string(),
        },
    }
}

#[tokio::test]
async fn should_accept_file_criterion_when_the_delivered_commit_holds_the_file() {
    let task = plain_task(vec![file_criterion("report.md")]);
    let Some(fx) = Fixture::with_task("broken\n", "true", task).await else {
        return;
    };
    let pool = fx
        .boot(Arc::new(PatchProvider::new("report.md", "done\n")), EPOCH)
        .await;

    let (_, outcome) = fx.run(&pool).await;

    let AttemptOutcome::Completed {
        verdict: AcceptanceVerdict::Accepted { evidence },
    } = &outcome
    else {
        panic!("expected an accepted completion, got {outcome:?}");
    };
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].kind, "file_exists");
    assert_eq!(
        evidence[0].locator,
        format!("{}:report.md", fx.git(&["rev-parse", BRANCH])),
        "the evidence names the file in the delivered commit",
    );
    assert_eq!(fx.clean_checkouts(), Vec::<String>::new());
}

#[tokio::test]
async fn should_reject_file_criterion_when_the_delivered_commit_lacks_the_file() {
    let task = plain_task(vec![file_criterion("report.md")]);
    let Some(fx) = Fixture::with_task("broken\n", "true", task).await else {
        return;
    };
    let pool = fx
        .boot(Arc::new(PatchProvider::new("value.txt", "fixed\n")), EPOCH)
        .await;

    let (_, outcome) = fx.run(&pool).await;

    let reason = rejection(&outcome);
    assert!(
        reason.contains("holds no regular file `report.md`"),
        "got: {reason}"
    );
}

#[tokio::test]
async fn should_reject_when_worktree_task_changes_nothing() {
    // The agent reports success without touching the tree. There is nothing to
    // deliver, whatever the acceptance says.
    let Some(fx) = Fixture::with_task("broken\n", "true", plain_task(vec![])).await else {
        return;
    };
    let pool = fx.boot(Arc::new(SuccessProvider), EPOCH).await;

    let (_, outcome) = fx.run(&pool).await;

    let reason = rejection(&outcome);
    assert!(
        reason.contains("no changes relative to base"),
        "got: {reason}"
    );
    assert_eq!(fx.child().await.status, ChildStatus::Failed);
}

// ---------------------------------------------------------------------------
// A verdict must not depend on what an earlier run left behind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_refuse_as_nothing_to_fix_when_base_test_passed_and_its_result_was_lost() {
    // The named test already PASSES on the base revision, and running it
    // refreshes a tracked file. The host dies after the test exits and before
    // its result is recorded. Nothing recorded says the test ever failed on the
    // base, so the file the test itself rewrote must not be taken for a fix.
    let Some(fx) = Fixture::with("initial\n", TEST_COMMAND, "echo refreshed > value.txt").await
    else {
        return;
    };
    let exited = kill_points::arm(&fx.store, KillPoint::AdmissionCommandExited);
    let (model, _) = uncalled_model();
    let boot1 = fx.boot(model, EPOCH).await;
    fx.start(&boot1).await;
    reached(exited, "the base run finished with its result not recorded").await;
    let lost = fx.journal(VerificationPhase::Admission).await;
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].result, None, "the admission outcome is unknown");

    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot2).await;

    let reason = rejection(&outcome);
    assert!(
        reason.contains("already passes on base revision"),
        "got: {reason}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    assert_eq!(fx.child().await.status, ChildStatus::Failed);
    let admission = fx.journal(VerificationPhase::Admission).await;
    assert_eq!(
        admission.len(),
        2,
        "the lost run, then the one that decided"
    );
    assert_eq!(
        (
            admission[1].commit.as_str(),
            admission[1].tree_digest.as_str()
        ),
        (fx.base_commit.as_str(), fx.base_tree.as_str()),
        "the deciding run is on the base revision itself",
    );
    assert_eq!(admission[1].result.as_ref().unwrap().exit_status, Some(0));
    assert!(
        fx.journal(VerificationPhase::Acceptance).await.is_empty(),
        "nothing was offered for acceptance",
    );
}

#[tokio::test]
async fn should_refuse_recovered_work_when_no_run_ever_recorded_the_test_failing_on_base() {
    // However it came about, this is the state: an interrupted attempt's
    // directory holds a change, and the journal has NO result for the named
    // test on the base revision. The test in fact passes there. Work being
    // present proves nothing about the base, so the base is checked before the
    // recovered work is — and the task is refused, not delivered.
    let Some(fx) = Fixture::with("initial\n", TEST_COMMAND, "echo refreshed > value.txt").await
    else {
        return;
    };
    let dead_attempt = launch(&fx.store, FLEET, TASK).await;
    fx.store.mark_running(TASK, &dead_attempt).await.unwrap();
    let dead = octos_core::new_work_checkout(&fx.stem()).unwrap();
    octos_core::prepare_fleet_worktree(fx.repo.path(), fx.work.path(), BRANCH, &dead).unwrap();
    assert!(
        StdCommand::new("git")
            .arg("-C")
            .arg(&dead)
            .args(["reset", "-q", "--hard"])
            .status()
            .unwrap()
            .success(),
        "the dead attempt populated its checkout",
    );
    fx.store
        .bind_worktree(
            FLEET,
            TASK,
            &dead_attempt,
            EPOCH,
            WorktreeBindRequest {
                repo_root: fx.repo.path().display().to_string(),
                branch: BRANCH.to_string(),
                checkout: dead.display().to_string(),
                base_commit: fx.base_commit.clone(),
                base_tree: fx.base_tree.clone(),
            },
            NOW,
        )
        .await
        .unwrap();
    fx.store
        .mark_worktree_populated(
            FLEET,
            TASK,
            &dead_attempt,
            EPOCH,
            &dead.display().to_string(),
            NOW,
        )
        .await
        .unwrap();
    std::fs::write(dead.join("value.txt"), "refreshed\n").unwrap();
    assert!(fx.journal(VerificationPhase::Admission).await.is_empty());

    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot2).await;

    let reason = rejection(&outcome);
    assert!(
        reason.contains("already passes on base revision"),
        "got: {reason}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    assert!(
        fx.journal(VerificationPhase::Acceptance).await.is_empty(),
        "the recovered work was never offered for acceptance",
    );
    assert_eq!(
        std::fs::read_to_string(dead.join("value.txt")).unwrap(),
        "refreshed\n",
        "and it was not touched",
    );
}

#[tokio::test]
async fn should_reject_when_patch_only_passes_with_a_cache_an_earlier_run_left_behind() {
    // The test fails the first time it runs in a directory and leaves a
    // git-ignored cache that makes every later run there pass. The agent's
    // change is unrelated. In a fresh checkout of the delivered commit the test
    // still fails, so the commit must not be accepted.
    let Some(fx) = Fixture::with(
        "broken\n",
        TEST_COMMAND,
        "if test -f test-cache; then exit 0; fi\necho cached > test-cache\nexit 1",
    )
    .await
    else {
        return;
    };
    std::fs::write(fx.repo.path().join(".git/info/exclude"), "test-cache\n").unwrap();
    let pool = fx
        .boot(
            Arc::new(PatchProvider::new("notes.txt", "unrelated\n")),
            EPOCH,
        )
        .await;

    let (_, outcome) = fx.run(&pool).await;

    let reason = rejection(&outcome);
    assert!(reason.contains("acceptance failed"), "got: {reason}");
    assert_eq!(fx.child().await.status, ChildStatus::Failed);
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(acceptance.len(), 1);
    assert_eq!(
        acceptance[0].result.as_ref().unwrap().exit_status,
        Some(1),
        "the candidate commit fails the test where nothing was left behind",
    );
}

#[tokio::test]
async fn should_test_the_commits_own_bytes_when_a_filter_is_planted_in_the_repository() {
    // A worker can write the task repository's `.git/config` and
    // `.git/info/attributes`. Here they hold a filter that makes `value.txt`
    // read `fixed` wherever a commit is verified — and nowhere else — while the
    // commit boot 1 left behind says `still-broken`. Accepting it would hand
    // out a receipt for content that was never tested.
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "still-broken\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(patch_written, "the patch on disk").await;
    fx.git(&[
        "config",
        "filter.planted.smudge",
        r#"case "$PWD" in *.verify-*) printf 'fixed\n';; *) cat;; esac"#,
    ]);
    std::fs::write(
        fx.repo.path().join(".git/info/attributes"),
        "value.txt filter=planted\n",
    )
    .unwrap();

    fx.reconcile(EPOCH + 1).await;
    let model = PatchProvider::new("value.txt", "fixed\n");
    let calls = model.calls.clone();
    let boot2 = fx.boot(Arc::new(model), EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot2).await;

    // The recovered commit was tested as it is and failed; the model then
    // finished the work, and what was accepted is what the branch holds.
    receipt_ops(&outcome);
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(
        acceptance[0].result.as_ref().unwrap().exit_status,
        Some(1),
        "the recovered commit does not hold the fix, whatever the filter writes",
    );
    assert!(calls.load(Ordering::SeqCst) >= 1, "so the model was called");
    assert_eq!(fx.git(&["show", &format!("{BRANCH}:value.txt")]), "fixed");
}

// ---------------------------------------------------------------------------
// A process that outlived its host
// ---------------------------------------------------------------------------

/// A real process that keeps appending to `stray.log` in `dir`, by absolute
/// path, until the test ends — what a build or a script the dead attempt
/// started looks like to its successor. Nobody tells it that its host is gone.
struct StrayWriter(std::process::Child);

impl StrayWriter {
    fn start(dir: &std::path::Path) -> Self {
        let log = dir.join("stray.log");
        let child = StdCommand::new("sh")
            .arg("-c")
            .arg(format!(
                "while :; do echo stray >> '{}' 2>/dev/null; sleep 0.02; done",
                log.display()
            ))
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the stray writer");
        // It is at work before the host "dies".
        let deadline = std::time::Instant::now() + REACH;
        while !log.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the stray writer never wrote"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Self(child)
    }
}

impl Drop for StrayWriter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn should_deliver_recovered_work_when_a_process_of_the_dead_attempt_keeps_writing() {
    // The host dies with the patch on disk — and a process the dead attempt
    // started is still alive, writing into that attempt's checkout. The lease
    // fences the dead attempt's store writes, not its files. The successor must
    // not work where that process writes: it takes what the checkout held and
    // continues somewhere the old process has no reach.
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
    let boot1 = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot1).await;
    reached(patch_written, "the patch on disk").await;
    let dead_checkout = fx.bound_checkout().await;
    let _stray = StrayWriter::start(&dead_checkout);

    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (_, outcome) = fx.run(&boot2).await;

    receipt_ops(&outcome);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    assert_eq!(fx.git(&["show", &format!("{BRANCH}:value.txt")]), "fixed");
    assert_ne!(
        fx.bound_checkout().await,
        dead_checkout,
        "the successor did not work in the dead attempt's directory",
    );
    assert_eq!(
        fx.journal(VerificationPhase::Acceptance).await.len(),
        1,
        "the recovered commit was verified once: nothing moved it afterwards",
    );
}

#[tokio::test]
async fn should_remove_a_dead_attempts_leftover_directories_when_the_task_runs_again() {
    // The host dies while a check is running: the dead attempt leaves its work
    // directory AND the clean checkout the check ran in. The successor recovers
    // the first and has no use for the second. Even when the successor's own
    // attempt is not accepted — so nothing is cleaned up on completion — only
    // one directory is left: the one holding the work.
    let Some(fx) = Fixture::new("broken\n").await else {
        return;
    };
    let exited = kill_points::arm(&fx.store, KillPoint::AcceptanceCommandExited);
    let boot1 = fx
        .boot(
            Arc::new(PatchProvider::new("value.txt", "halfway\n")),
            EPOCH,
        )
        .await;
    fx.start(&boot1).await;
    reached(exited, "a finished check whose result is not committed").await;
    let dead_checkout = fx.bound_checkout().await;
    assert_eq!(fx.clean_checkouts().len(), 1);

    fx.reconcile(EPOCH + 1).await;
    let boot2 = fx
        .boot(
            Arc::new(PatchProvider::new("value.txt", "still wrong\n")),
            EPOCH + 1,
        )
        .await;
    let (_, outcome) = fx.run(&boot2).await;

    rejection(&outcome);
    let kept = fx.bound_checkout().await;
    assert_ne!(kept, dead_checkout);
    assert_eq!(
        fx.task_dirs(""),
        vec![kept.file_name().unwrap().to_string_lossy().into_owned()],
        "only the directory holding the unaccepted work is left",
    );
    assert_eq!(
        std::fs::read_to_string(kept.join("value.txt")).unwrap(),
        "still wrong\n",
    );
}

// ---------------------------------------------------------------------------
// A host that is really killed
// ---------------------------------------------------------------------------

/// Set (to the fixture root) in the process that plays the host about to be
/// killed; see [`doomed_host`].
const DOOMED_HOST: &str = "OCTOS_FLEET_TEST_DOOMED_HOST";

/// The one test [`Host::start`] runs in the host process.
const KILLED_HOST_TEST: &str =
    "should_recover_work_with_zero_model_calls_when_the_host_process_is_killed";

/// The first boot, in a process of its own: build the fixture under `root`,
/// run the task until the patch is on disk with a model call outstanding, leave
/// a process of the attempt's running in its checkout, report, and wait to be
/// killed. Never returns.
async fn doomed_host(root: &Path) -> ! {
    let fx = Fixture::create_under(
        root,
        "broken\n",
        "grep -q fixed value.txt",
        failing_test_task(TEST_COMMAND),
    )
    .await
    .expect("git is available: the parent checked");
    let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
    let boot = fx.boot(Arc::new(model), EPOCH).await;
    fx.start(&boot).await;
    reached(patch_written, "the patch on disk").await;
    let stray = StrayWriter::start(&fx.bound_checkout().await);

    // Report in one step, so the parent never reads half a pid.
    let ready = root.join("host-ready");
    std::fs::write(ready.with_extension("tmp"), stray.0.id().to_string()).unwrap();
    std::fs::rename(ready.with_extension("tmp"), &ready).unwrap();
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// The host process and, once it has reported one, the process its attempt
/// left running. Neither may outlive the test, however the test ends.
struct Host {
    process: std::process::Child,
    root: PathBuf,
}

impl Host {
    /// Run this test binary again as the host of the fixture under `root`.
    fn start(root: &Path) -> Self {
        let log = std::fs::File::create(root.join("host.log")).unwrap();
        let test = format!(
            "{}::{KILLED_HOST_TEST}",
            module_path!()
                .split_once("::")
                .expect("a module inside the crate")
                .1
        );
        let process = StdCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture", "--test-threads=1"])
            .env(DOOMED_HOST, root)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("start the host process");
        Self {
            process,
            root: root.to_path_buf(),
        }
    }

    /// The pid of the process the host's attempt left running, once the host
    /// has reported it.
    fn stray_pid(&self) -> Option<String> {
        std::fs::read_to_string(self.root.join("host-ready")).ok()
    }

    /// Wait until the host has the patch on disk and a process running in its
    /// checkout; that process's pid.
    async fn ready(&mut self) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(pid) = self.stray_pid() {
                return pid;
            }
            let ended = self.process.try_wait().unwrap();
            assert!(
                ended.is_none() && std::time::Instant::now() < deadline,
                "the host process never got the patch on disk (ended: {ended:?}):\n{}",
                std::fs::read_to_string(self.root.join("host.log")).unwrap_or_default(),
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        if let Some(pid) = self.stray_pid() {
            let _ = StdCommand::new("kill")
                .args(["-9", &pid])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// Whether the process `pid` exists.
fn alive(pid: &str) -> bool {
    StdCommand::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test]
async fn should_recover_work_with_zero_model_calls_when_the_host_process_is_killed() {
    // Every test above models the host's death inside one process. Here it is
    // real: another OS process runs the first boot and is killed with SIGKILL —
    // no destructor, no drop-guard, no shutdown hook, a database nobody closed.
    // The process its attempt started is orphaned and keeps writing. THIS
    // process is then the next boot, on the store and the repository the dead
    // one left.
    use std::os::unix::process::ExitStatusExt;

    if let Some(root) = std::env::var_os(DOOMED_HOST) {
        doomed_host(Path::new(&root)).await;
    }
    if StdCommand::new("git").arg("--version").output().is_err() {
        return;
    }
    let root = TempDir::new().unwrap();
    let mut host = Host::start(root.path());
    let stray = host.ready().await;

    host.process.kill().unwrap(); // SIGKILL
    let status = host.process.wait().unwrap();
    assert_eq!(status.signal(), Some(9), "the host was killed: {status}");

    // What the killed process left behind.
    let fx = Fixture::reopen_under(root.path()).await;
    let dead_checkout = fx.bound_checkout().await;
    assert_eq!(
        fx.child().await.status,
        ChildStatus::Running,
        "the store still says the task is running",
    );
    assert_eq!(
        std::fs::read_to_string(dead_checkout.join("value.txt")).unwrap(),
        "fixed\n",
    );
    assert_eq!(
        fx.git(&["rev-parse", BRANCH]),
        fx.base_commit,
        "the patch was never committed",
    );
    assert_eq!(fx.journal(VerificationPhase::Admission).await.len(), 1);
    let written = || {
        std::fs::metadata(dead_checkout.join("stray.log"))
            .map(|log| log.len())
            .unwrap_or(0)
    };
    let before = written();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        alive(&stray) && written() > before,
        "the process the dead attempt started outlived its host and is still writing",
    );

    // The next boot: reconcile, dispatch — with a model that must not be called.
    fx.reconcile(EPOCH + 1).await;
    let (model, calls) = uncalled_model();
    let boot2 = fx.boot(model, EPOCH + 1).await;
    let (attempt, outcome) = fx.run(&boot2).await;

    let ops = receipt_ops(&outcome);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
    let acceptance = fx.journal(VerificationPhase::Acceptance).await;
    assert_eq!(
        acceptance.len(),
        1,
        "the recovered commit was verified once: nothing moved it afterwards",
    );
    let receipt = &acceptance[0];
    assert_eq!(ops, vec![receipt.op_id.clone()]);
    assert_eq!(receipt.attempt_id, attempt);
    assert_eq!(receipt.commit, fx.git(&["rev-parse", BRANCH]));
    assert_eq!(fx.git(&["show", &format!("{BRANCH}:value.txt")]), "fixed");
    assert_eq!(fx.child().await.status, ChildStatus::Succeeded);
    assert_eq!(
        fx.journal(VerificationPhase::Admission).await.len(),
        1,
        "the dead host's admission result was found, not repeated",
    );
    assert_eq!(
        fx.runs(),
        2,
        "the test ran twice in all: on the base by the host that was killed, and on the \
         recovered commit by its successor",
    );
    assert_ne!(
        fx.bound_checkout().await,
        dead_checkout,
        "the successor did not work in the dead attempt's directory",
    );
}

// ---------------------------------------------------------------------------
// The real macOS sandbox
// ---------------------------------------------------------------------------

/// Everything above runs under a test-double sandbox that confines nothing.
/// Here the worktree flow runs under the real `sandbox-exec` backend, built
/// the way a server builds it for a fleet worker: reads restricted to an
/// allow-list, and the task's grant folded on for each directory it works in.
#[cfg(target_os = "macos")]
mod real_macos_sandbox {
    use octos_agent::sandbox::{Sandbox, SandboxConfig, SandboxMode, create_sandbox};

    use super::*;

    /// A fixture root the DEFAULT read allow-list does not cover (the system
    /// temp directories are on it), so the sandbox really cannot read the
    /// repository unless a rule lets it — like a repository under a user's
    /// home directory.
    fn unlisted_root() -> TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sandbox-tests");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(std::fs::canonicalize(base).unwrap()).unwrap()
    }

    async fn boot(fx: &Fixture, provider: Arc<dyn LlmProvider>, epoch: u64) -> FleetWorkerPool {
        let (_memory_dir, factory) = factory_for_with(
            provider,
            Arc::new(|_cwd, grant| {
                Arc::from(create_sandbox(&SandboxConfig {
                    mode: SandboxMode::Macos,
                    read_allow_paths: vec!["/usr".into()],
                    allow_network: grant.allow_network,
                    repo_git_write: grant.repo_git_dir,
                    write_allow_globs: grant.write_allow_globs,
                    ..Default::default()
                })) as Arc<dyn Sandbox>
            }),
        )
        .await;
        fx.boot_with(factory, epoch)
    }

    #[tokio::test]
    async fn should_recover_and_deliver_under_the_real_sandbox_when_reads_are_restricted() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let root = unlisted_root();
        let Some(fx) = Fixture::create_under(
            root.path(),
            "broken\n",
            "grep -q fixed value.txt",
            failing_test_task(TEST_COMMAND),
        )
        .await
        else {
            return;
        };

        // Boot 1: admitted on the base, the patch written, then the host dies.
        let (model, patch_written) = PatchProvider::then_never_answers("value.txt", "fixed\n");
        let boot1 = boot(&fx, Arc::new(model), EPOCH).await;
        fx.start(&boot1).await;
        reached(patch_written, "the patch on disk").await;
        let dead_checkout = fx.bound_checkout().await;
        let admission = fx.journal(VerificationPhase::Admission).await;
        assert_eq!(admission.len(), 1);
        assert_eq!(
            admission[0].result.as_ref().unwrap().exit_status,
            Some(1),
            "the test really ran, and failed, on the base",
        );

        // Boot 2: the dead attempt's work is committed, verified and delivered.
        fx.reconcile(EPOCH + 1).await;
        let (model, calls) = uncalled_model();
        let boot2 = boot(&fx, model, EPOCH + 1).await;
        let (_, outcome) = fx.run(&boot2).await;

        let ops = receipt_ops(&outcome);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
        let acceptance = fx.journal(VerificationPhase::Acceptance).await;
        assert_eq!(acceptance.len(), 1);
        assert_eq!(ops, vec![acceptance[0].op_id.clone()]);
        assert_eq!(acceptance[0].result.as_ref().unwrap().exit_status, Some(0));
        assert_eq!(fx.git(&["show", &format!("{BRANCH}:value.txt")]), "fixed");
        assert!(!dead_checkout.exists(), "the dead directory was retired");
    }
}
