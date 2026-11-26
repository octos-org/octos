//! Shared `git worktree` plumbing for worktree-isolated task workers.
//!
//! Lifted (in spirit) from the spawn-tool worktree mechanism in
//! `octos-agent`'s `tools/spawn.rs` into the leaf `octos-core` crate so
//! `octos-fleet-worker`'s pool can allocate a REAL `git worktree` of a
//! controller repository for each fleet task WITHOUT taking a dependency on
//! `octos-agent`. This module is purely ADDITIVE: `spawn.rs` keeps its own
//! (heavily reviewed) copy and is deliberately NOT rewired onto this module,
//! so lifting the helpers here cannot regress that path. A later PR may dedupe.
//!
//! # SECURITY — a worktree worker is a COHERENT full-trust worker
//!
//! A worktree worker runs ONLY when the operator granted the task a COHERENT
//! full-trust grant — `FsGrant::Host` AND `NetworkGrant::Full` (the pool gate;
//! projected fresh per attempt). This removes the trust GRADIENT that made the
//! parked design fragile: full FS write (the sandbox's `repo_git_write` — see
//! [`octos_agent::sandbox::SandboxConfig::repo_git_write`]) lets a worker bridge
//! ANY lesser network fence (host `AF_UNIX` sockets survive `--unshare-net`; a
//! planted `.git` filter runs on a controller git op), so a `Host-FS +
//! restricted-network` worker is NOT truly isolated. Rather than fence deeper
//! (whack-a-mole), the worktree path REQUIRES full network too — then bridging
//! gains the worker nothing (it already has full network), and the coarse
//! operator grant IS the trust decision. This drops the parked
//! `.git`-write-with-hook/config-deny-fence: a full-trust worker already has the
//! whole FS + network, so micro-carving `.git` bought nothing. (A
//! network-ISOLATED worktree worker WOULD need that deny-fence — a DEFERRED
//! follow-up; v1's worktree = a full-permission worker.)
//!
//! Two dissolutions keep even a full-trust worker from escalating the CONTROLLER
//! (the trusted daemon, a HIGHER trust level than the worker):
//!
//! - **(a) controller-side git hardening:** EVERY controller-side git op
//!   (worktree add/remove/prune, ref reads) goes through [`git_command`], which
//!   (1) invokes an ABSOLUTE `git` ([`GIT_BIN`], never `$PATH` — so a
//!   worker-planted `git` in a controller-`$PATH` dir is never run), (2) STRIPS
//!   provider/API-key + injection env vars ([`crate::env_hygiene`] — so no
//!   controller secret is inherited), and (3) sets hooks
//!   (`core.hooksPath=<null device>`), fsmonitor (`core.fsmonitor=`), and
//!   global/system config
//!   (`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM=<null device>`) to empty, so no
//!   hook/fsmonitor fires and no global/system-config `filter.*` runs on a
//!   controller op. The null device is [`NULL_DEVICE`] — `/dev/null`, or `NUL`
//!   on Windows.
//! - **(b) no host-side checkout + in-sandbox populate/commit:** every controller
//!   `git worktree add` uses `--no-checkout`, so a worker-planted LOCAL
//!   `.git/config` `filter.*.smudge` never runs host-side as the daemon. The
//!   worker POPULATES its working tree ([`worktree_populate_command`],
//!   `git reset --hard`) and lands the deliverable ([`deliverable_commit_command`])
//!   INSIDE its own sandbox, so a planted clean/smudge filter or hook runs at the
//!   WORKER's grant — contained, never host-side.
//!
//! Containing a planted filter says where it runs, not what it does to the
//! files. So the checkout a commit is VERIFIED in is not a worktree of the task
//! repository at all: [`clean_checkout_command`] makes it a repository of its
//! own that borrows only the object store, and no configuration, attribute
//! file or hook a worker can write is read there.
//!
//! The worker can still create/move/**delete any ref** — the `fleet/<fleet>/
//! <task>` namespace is a CONVENTION, not a sandbox fence. RESIDUAL (narrow,
//! accepted): a user who LATER manually checks out an untrusted `fleet/*` branch
//! with their own (unmasked) filters runs whatever it committed — the standard
//! untrusted-branch review risk, outside this sandbox's scope.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use eyre::{Result, WrapErr, eyre};

/// The platform's null device: a path that can never be a directory and never
/// holds config. `/dev/null` on Unix, `NUL` on Windows — a reserved device name,
/// so it is the exact analogue.
///
/// On Windows the old hardcoded `/dev/null` did still mask config and hooks, but
/// only by ACCIDENT: it is not the null device there, merely a path that happens
/// not to exist. That is a property of the filesystem, not a guarantee — create
/// `C:\dev\null` and the fence silently opens. `NUL` cannot be a directory or a
/// readable config no matter what is on disk.
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";

/// `-c core.hooksPath=<null device>`: disables ALL git hooks for a
/// controller-side invocation (the null device can never be a hooks directory,
/// so no hook is ever found). One part of the code-exec fence — see the module
/// note and [`git_command`].
#[cfg(not(windows))]
const NO_HOOKS: &str = "core.hooksPath=/dev/null";
#[cfg(windows)]
const NO_HOOKS: &str = "core.hooksPath=NUL";

/// `-c core.fsmonitor=`: override any worker-set local `core.fsmonitor` to EMPTY
/// so no fsmonitor hook program can run on a controller-side git op (defensive
/// belt — these ops don't scan the index, but override unconditionally).
const NO_FSMONITOR: &str = "core.fsmonitor=";

/// Candidate absolute `git` paths, most-preferred first. Every entry MUST be a
/// location a fleet worker cannot write to (see [`GIT_BIN`]).
///
/// On Windows the analogue of `/usr/bin` is `%ProgramFiles%\Git`, which requires
/// Administrator to write. `%LOCALAPPDATA%\Programs\Git` — where Git for Windows
/// installs in per-user mode — is deliberately EXCLUDED for exactly the reason
/// Homebrew is on Unix: it is user-writable, which is the plant vector. A host
/// with only a per-user git therefore fails closed (git not found) rather than
/// running a binary the worker could have replaced.
#[cfg(not(windows))]
const GIT_CANDIDATES: &[&str] = &["/usr/bin/git", "/bin/git"];
#[cfg(windows)]
const GIT_CANDIDATES: &[&str] = &[
    r"C:\Program Files\Git\cmd\git.exe",
    r"C:\Program Files\Git\bin\git.exe",
    r"C:\Program Files (x86)\Git\cmd\git.exe",
    r"C:\Program Files (x86)\Git\bin\git.exe",
];

/// Absolute path to the `git` binary, resolved ONCE from NON-worker-writable
/// system locations — NEVER via `$PATH`. A fleet worker's NATIVE tools have
/// full-FS (`FilesystemScope::Host`) write, so a bare `git` (PATH-resolved) would
/// let it plant a fake `git` earlier in the controller's `$PATH` (or in a
/// user-writable dir like `/opt/homebrew/bin`) and have the CONTROLLER, running
/// UNSANDBOXED, execute it. Pin to [`GIT_CANDIDATES`] (root-owned / SIP-protected
/// on Unix, Administrator-only on Windows — not worker-writable); the fallback
/// stays ABSOLUTE so there is never a `$PATH` lookup. Mirrors the timeout-kill's
/// `KILL_BIN`/`PS_BIN`. (Homebrew/`/usr/local` are deliberately NOT candidates —
/// they are user/daemon-writable, which is exactly the plant vector.)
static GIT_BIN: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
    GIT_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        // Keep the fallback ABSOLUTE: a missing git must fail as "cannot spawn
        // this exact path", never degrade into a `$PATH` lookup.
        .unwrap_or_else(|| PathBuf::from(GIT_CANDIDATES[0]))
});

/// Build the base `git -C <repo> …` command with the controller-side code-exec
/// AND secret-exfiltration hardening applied, so a lower-trust worker cannot
/// hijack a CONTROLLER-side git op running OUTSIDE the sandbox:
///
/// - **binary**: an ABSOLUTE `git` ([`GIT_BIN`]), never `$PATH`-resolved, so a
///   worker-planted `git` in a controller-`$PATH` dir is never invoked.
/// - **env**: provider/API-key + injection vars are STRIPPED
///   ([`crate::env_hygiene::sanitize_git_command_env`]) so even a config-driven
///   exec could not read a controller secret (defense in depth).
/// - **hooks**: `-c core.hooksPath=/dev/null` — no hook fires on any controller
///   op (`worktree add`'s post-checkout, etc.).
/// - **fsmonitor**: `-c core.fsmonitor=` — a worker-set local fsmonitor program
///   is overridden to empty (belt).
/// - **global** and **system** config: masked to `/dev/null` via
///   `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM`, so a worker that wrote a
///   `~/.gitconfig` (or `/etc/gitconfig`) `filter.*` can't influence a controller
///   op either.
///
/// The worker's OWN deliverable commit deliberately does NOT go through this
/// builder — it runs INSIDE the worker's sandbox (dissolution (b)) so a planted
/// local-config filter runs at the worker's (contained) network grant. EVERY
/// controller-side git invocation MUST go through this builder.
fn git_command(repo: &Path) -> Command {
    let mut cmd = Command::new(&*GIT_BIN);
    cmd.arg("-C")
        .arg(repo)
        .arg("-c")
        .arg(NO_HOOKS)
        .arg("-c")
        .arg(NO_FSMONITOR);
    // Strip EVERY provider secret (heuristic + runtime-registered, e.g.
    // `VERTEX_SA_JSON`) + injection var from the inherited controller env — the
    // SAME set the worker-sandboxed git ops strip. Done BEFORE re-applying the
    // git-specific env below, so those overrides can never be stripped.
    crate::env_hygiene::sanitize_git_command_env(&mut cmd);
    cmd.env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_CONFIG_SYSTEM", NULL_DEVICE);
    cmd
}

/// Whether `dir` is inside a git work tree. A convenience over [`probe_git_repo`]
/// that treats ANY probe failure (missing binary, permission error) the same as
/// "not a repo" — use [`probe_git_repo`] where a probe error must be
/// distinguished from a confirmed non-repo (the pool does, so it never runs a
/// REAL repo in scratch mode just because the probe failed).
pub fn is_git_repo(dir: &Path) -> bool {
    probe_git_repo(dir).unwrap_or(false)
}

/// Probe whether `dir` is inside a git work tree, DISTINGUISHING a confirmed
/// non-repo from a probe ERROR (MEDIUM #7):
///
/// - `Ok(true)` — git confirms `dir` is inside a work tree.
/// - `Ok(false)` — a DEFINITE negative: the root is absent, or git ran and said
///   "not a git repository" (or printed `false` for a bare `.git`). Scratch
///   fallback is correct.
/// - `Err(_)` — the probe itself FAILED (git missing / spawn failure /
///   permission / not-a-directory / any other non-"not a repository" error), so
///   we CANNOT conclude it is not a repo. The caller must propagate this rather
///   than silently scratch a real repository.
pub fn probe_git_repo(dir: &Path) -> Result<bool> {
    // An absent root is a definite non-repo (scratch is correct); avoid a
    // confusing "cannot change to ...: No such file or directory" probe error.
    if !dir.exists() {
        return Ok(false);
    }
    let out = git_command(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .wrap_err_with(|| format!("failed to spawn git to probe {}", dir.display()))?;
    if out.status.success() {
        // "true" = inside a work tree; "false" = inside a bare `.git` (not a
        // work tree) — either way a definite answer.
        return Ok(String::from_utf8_lossy(&out.stdout).trim() == "true");
    }
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    if stderr.contains("not a git repository") {
        return Ok(false); // confirmed non-repo
    }
    // Any OTHER failure (permission denied, not-a-directory, corrupt repo, …)
    // is a probe error, NOT a confirmed non-repo: propagate.
    Err(eyre!(
        "git repository probe of {} failed: {}",
        dir.display(),
        stderr.trim()
    ))
}

/// Whether `refname` (e.g. `refs/heads/fleet/f/t`) resolves in `repo`.
pub fn git_ref_exists(repo: &Path, refname: &str) -> Result<bool> {
    let status = git_command(repo)
        .args(["show-ref", "--verify", "--quiet", refname])
        .status()
        .wrap_err("failed to run git show-ref")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(eyre!("git show-ref exited with {status}")),
    }
}

/// A prepared `git worktree` checkout for one fleet task.
#[derive(Debug, Clone)]
pub struct PreparedWorktree {
    /// Canonical controller repository root.
    pub repo_root: PathBuf,
    /// The TASK-STABLE branch checked out (`fleet/<fleet>/<task>`). Kept as the
    /// deliverable on completion; the checkout is removed, the branch is not.
    pub branch: String,
    /// The worktree checkout directory — the worker's cwd.
    pub checkout: PathBuf,
    /// The repository's common git directory (`<repo>/.git` for a normal repo),
    /// canonical + absolute. Under `FsGrant::Host` the worker sandbox rw-binds
    /// exactly THIS path (`repo_git_write`), so `git commit` inside the checkout
    /// can reach it (objects/refs/logs live here, outside the checkout) — the
    /// pool threads this into the sandbox grant + worktree admin plumbing.
    pub git_dir: PathBuf,
    /// This checkout's PER-WORKTREE admin dir (`<git_dir>/worktrees/<slug>`),
    /// where its HEAD/index/logs live. Resolved from the linked worktree's
    /// `.git` pointer file.
    pub worktree_git_dir: PathBuf,
    /// The branch's head commit right after prepare.
    pub base_commit: String,
}

/// Create a worktree checkout of a fleet task's branch at `checkout`.
///
/// `work_root` is the pool's `fleet-work` root; the `checkout` MUST resolve
/// safely under it (no symlinked component, no escape — [`assert_checkout_contained`]),
/// which is what makes the removals here safe (HIGH #2: never create or
/// `remove_dir_all` outside `fleet-work`).
///
/// The BRANCH (`fleet/<fleet>/<task>`) is stable across attempts — it is what
/// carries a task's committed work from one attempt to the next. The checkout
/// DIRECTORY is not: every attempt gets a directory of its own
/// ([`new_work_checkout`]), so nothing still running in an earlier attempt's
/// directory can write into this one.
///
/// 1. Whatever sits at `checkout` (normally nothing) is freed first by
///    [`force_free_checkout`] — even a LOCKED worktree (HIGH #5) — WITHOUT
///    deleting the branch.
/// 2. If the branch exists, `git worktree add --force <checkout> <branch>` (NO
///    `-b`), so the checkout continues from the branch's last commit;
///    otherwise `git worktree add -b <branch> <checkout> HEAD` for a fresh task.
///    `--force` because the branch may still be checked out in the directory of
///    an interrupted attempt whose work has yet to be recovered: this checkout
///    is created EMPTY beside it and is only populated after the caller has
///    committed that work and retired that directory.
///
/// The branch is NEVER deleted here — it is the deliverable. Every git op runs
/// hooks-disabled (module SECURITY note).
pub fn prepare_fleet_worktree(
    repo_root: &Path,
    work_root: &Path,
    branch: &str,
    checkout: &Path,
) -> Result<PreparedWorktree> {
    validate_fleet_branch(branch)?;
    if !is_git_repo(repo_root) {
        return Err(eyre!(
            "controller workspace {} is not a git repository",
            repo_root.display()
        ));
    }
    let canonical_root = std::fs::canonicalize(repo_root)
        .wrap_err_with(|| format!("failed to canonicalize repo root {}", repo_root.display()))?;
    let git_dir = git_common_dir(&canonical_root)?;

    // HIGH #2: prove the checkout is safely contained in `fleet-work` with NO
    // symlinked component in its parent chain, BEFORE any create/remove. This
    // both refuses a swapped-in symlink at the checkout (which would redirect
    // the worker's writes) and guarantees no reconcile removal can escape
    // `fleet-work` into unrelated data or the controller repo.
    assert_checkout_contained(work_root, checkout)?;

    // Ensure the parent exists so `git worktree add` can create the leaf (the
    // parent chain is now proven symlink-free + contained).
    if let Some(parent) = checkout.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create worktree parent {}", parent.display()))?;
    }

    // Provision a commit identity so a headless worker's plain `git commit`
    // works (HIGH #4). Only set when the repo has NONE, so a user's configured
    // identity is never clobbered; the controller auto-commit passes `-c user.*`
    // regardless, so this is a convenience, not the deliverable guarantee.
    provision_commit_identity(&canonical_root);

    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = git_ref_exists(&canonical_root, &branch_ref)?;
    let checkout_exists = checkout.symlink_metadata().is_ok();

    // Reconcile a leftover checkout/branch from a prior (interrupted) attempt —
    // robust to a LOCKED leftover (HIGH #5).
    if checkout_exists || branch_exists {
        force_free_checkout(&canonical_root, Some(&git_dir), work_root, checkout);
    }

    // Re-add. Keep the branch (resume from its last commit) if it exists;
    // otherwise create it off HEAD.
    //
    // `--no-checkout` (defence-in-depth, module SECURITY note): the controller's
    // `worktree add` sets HEAD + the admin files but does NOT write the working
    // tree, so a worker-planted local `.git/config` `filter.*.smudge` +
    // `.git/info/attributes` cannot execute HOST-SIDE (as the daemon) during the
    // checkout. The worker POPULATES the working tree itself INSIDE its sandbox
    // (`git reset --hard`, at the worker's grant) — see the worktree worker's
    // populate step — so any smudge filter runs contained.
    let checkout_str = checkout.to_string_lossy();
    if branch_exists {
        run_git(
            &canonical_root,
            &[
                "worktree",
                "add",
                "--force",
                "--no-checkout",
                &checkout_str,
                branch,
            ],
        )
        .wrap_err("git worktree add (resume existing fleet branch) failed")?;
    } else {
        run_git(
            &canonical_root,
            &[
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                branch,
                &checkout_str,
                "HEAD",
            ],
        )
        .wrap_err("git worktree add (fresh fleet branch) failed")?;
    }

    // The branch head right after prepare — the base for the deliverable check.
    let base_commit = run_git(&canonical_root, &["rev-parse", branch]).unwrap_or_default();

    // This checkout's PER-WORKTREE admin dir (`<git_dir>/worktrees/<slug>`),
    // read from the linked worktree's `.git` pointer file (fall back to the
    // conventional slug = checkout basename).
    let worktree_git_dir = worktree_admin_dir(checkout).unwrap_or_else(|| {
        let slug = match checkout.file_name() {
            Some(name) => name,
            None => checkout.as_os_str(),
        };
        git_dir.join("worktrees").join(slug)
    });

    Ok(PreparedWorktree {
        repo_root: canonical_root,
        branch: branch.to_string(),
        checkout: checkout.to_path_buf(),
        git_dir,
        worktree_git_dir,
        base_commit,
    })
}

/// What the directory a durable record names as holding a task's work turned
/// out to be. See [`inspect_recorded_checkout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedCheckout {
    /// Nothing is there any more — no path, or an empty directory. There is
    /// nothing to recover from it and nothing to lose by removing it.
    Gone,
    /// It is still a worktree of this repository that git works in, so
    /// [`recovery_snapshot_command`] can commit whatever it holds to the task
    /// branch — whichever branch, or none, its `HEAD` was left on.
    Recoverable,
    /// Something is there and git cannot commit from it; the string says why.
    /// It may be the only copy of the task's work, so it must be left exactly
    /// as it is: not swept, not reset, not replaced.
    Unrecoverable(String),
}

/// Look at `checkout`, the directory a durable record names as holding an
/// interrupted attempt's work, and say what can be done with it.
///
/// A caller that recovers work asks here before it does anything to the
/// task's directories. The answer is about committing FROM the directory,
/// never about working in it (a process of the dead attempt may still be
/// writing there).
///
/// [`RecordedCheckout::Recoverable`] needs ALL of these:
///
/// - `checkout` is an existing directory proven contained in `work_root` with
///   no symlinked component (the same [`assert_checkout_contained`] proof
///   `prepare_fleet_worktree` requires — a failure is an `Err`, not a fallback);
/// - git has it REGISTERED as a worktree of `repo_root`, not marked prunable;
/// - its `.git` pointer resolves to an admin dir under this repository's own
///   `<git_dir>/worktrees`.
///
/// Where its `HEAD` points is NOT one of them. An attempt that detached its
/// `HEAD` or switched to another branch before it died still left its work in
/// the directory, and the snapshot commits the directory's content to the task
/// branch whatever `HEAD` says.
///
/// This MUTATES NOTHING: no `worktree add`, no checkout, no index write. The
/// only git invocations are read-only and go through [`git_command`] (hooks
/// disabled, global/system config masked), so the controller-side code-exec
/// fence of the module SECURITY note holds unchanged.
///
/// It cannot tell a populated working tree from one the worker never populated
/// (`--no-checkout` leaves it empty of files, but with its `.git` pointer). The
/// caller must know that from its own durable record before trusting the
/// tree's contents.
pub fn inspect_recorded_checkout(
    repo_root: &Path,
    work_root: &Path,
    checkout: &Path,
) -> Result<RecordedCheckout> {
    if !is_git_repo(repo_root) {
        return Err(eyre!(
            "controller workspace {} is not a git repository",
            repo_root.display()
        ));
    }
    if checkout.symlink_metadata().is_err() {
        return Ok(RecordedCheckout::Gone);
    }
    assert_checkout_contained(work_root, checkout)?;
    let unrecoverable = |why: &str| Ok(RecordedCheckout::Unrecoverable(why.to_string()));
    if !checkout.is_dir() {
        return unrecoverable("it is not a directory");
    }
    let empty = std::fs::read_dir(checkout)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false);
    if empty {
        return Ok(RecordedCheckout::Gone);
    }

    let canonical_root = std::fs::canonicalize(repo_root)
        .wrap_err_with(|| format!("failed to canonicalize repo root {}", repo_root.display()))?;
    let git_dir = git_common_dir(&canonical_root)?;
    let Ok(canonical_checkout) = std::fs::canonicalize(checkout) else {
        return unrecoverable("its path cannot be resolved");
    };

    let listing = run_git(&canonical_root, &["worktree", "list", "--porcelain"])
        .wrap_err("git worktree list failed")?;
    let entry = listing.split("\n\n").find(|block| {
        block.lines().any(|line| {
            line.strip_prefix("worktree ").is_some_and(|path| {
                std::fs::canonicalize(path).is_ok_and(|path| path == canonical_checkout)
            })
        })
    });
    let Some(entry) = entry else {
        return unrecoverable("git no longer lists it as a worktree of the repository");
    };
    if entry
        .lines()
        .any(|line| line == "prunable" || line.starts_with("prunable "))
    {
        return unrecoverable("git lists it as a worktree whose `.git` file is missing or broken");
    }

    // The pointer file must lead back into THIS repository's worktree admin
    // area, so a commit made in the directory lands in this repository.
    let admin = worktree_admin_dir(checkout).and_then(|dir| std::fs::canonicalize(dir).ok());
    if !admin.is_some_and(|admin| admin.starts_with(git_dir.join("worktrees"))) {
        return unrecoverable("its `.git` file does not lead back to the repository");
    }
    Ok(RecordedCheckout::Recoverable)
}

/// Create `checkout` as a NEW, EMPTY directory: the place a commit's checks
/// run, where nothing an earlier run left behind (a build cache, a file git
/// ignores, a half-written edit) can be seen. The worker fills it inside its
/// own sandbox with [`clean_checkout_command`].
///
/// `checkout` must be proven contained in `work_root`, like every directory
/// this module creates or removes. Whatever already sits there is removed
/// first; a directory that cannot be emptied is an `Err`. No git runs here and
/// the directory is not registered with any repository. Remove it with
/// [`remove_checkout_keep_branch`].
pub fn prepare_clean_checkout_dir(work_root: &Path, checkout: &Path) -> Result<()> {
    assert_checkout_contained(work_root, checkout)?;
    if let Some(parent) = checkout.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create checkout parent {}", parent.display()))?;
    }
    remove_contained_dir(work_root, checkout);
    std::fs::create_dir(checkout)
        .wrap_err_with(|| format!("failed to create clean checkout {}", checkout.display()))
}

/// The POSIX `sh -c` command that turns the empty directory it runs in into a
/// checkout of exactly `commit`, whose files do not depend on anything a
/// worker can write into the task repository's `.git` directory.
///
/// The directory becomes a repository OF ITS OWN. `git init` gives it a new
/// `.git` with default configuration, and it borrows the task repository's
/// object store (`git_dir`, that repository's common `.git` directory) through
/// `objects/info/alternates` — nothing else. The task repository's
/// `.git/config`, `.git/info/attributes` and hooks are not read there, so a
/// `filter.*` driver, an attribute line or a hook planted in them cannot run
/// and cannot change one byte of the checked-out files. What the files hold is
/// decided by the commit alone: its blobs, and the conversions its own
/// committed `.gitattributes` ask for (line endings, `ident`,
/// `working-tree-encoding`). A filter the commit names has no driver here and
/// leaves the blob as it is.
///
/// `HEAD` is detached at `commit`, so nothing run there moves a ref by
/// committing; the task repository's branches are not visible at all.
///
/// `commit` must be a full object id, never a ref name: the caller checks out
/// the immutable object it read, not whatever a name resolves to now. `Err`
/// for anything else, and for a `git_dir` that cannot be written to an
/// alternates file (not absolute, not UTF-8, or holding a line break).
///
/// PLATFORM: POSIX `sh`, like [`deliverable_commit_command`]. The worker's
/// command builder masks the global and system configuration in the
/// environment.
pub fn clean_checkout_command(git_dir: &Path, commit: &str) -> Result<String> {
    validate_object_id(commit)?;
    let objects = git_dir.join("objects");
    let objects = objects
        .to_str()
        .filter(|path| git_dir.is_absolute() && !path.contains(['\n', '\r']))
        .ok_or_else(|| {
            eyre!(
                "the object store of {} cannot be named in an alternates file",
                git_dir.display()
            )
        })?;
    // A repository's object format is fixed when it is created, and it must
    // be the one the borrowed objects are stored in.
    let object_format = if commit.len() == 64 {
        " --object-format=sha256"
    } else {
        ""
    };
    Ok(format!(
        "git -c init.defaultBranch=detached init -q --template={object_format} . \
         && printf '%s\\n' {objects} > .git/objects/info/alternates \
         && git -c core.hooksPath=/dev/null -c advice.detachedHead=false \
         checkout -q --detach {commit}",
        objects = sh_single_quote(objects),
    ))
}

/// Whether `commit`'s tree holds a REGULAR FILE at the repository-relative
/// `path`. A directory, a symlink, a submodule or nothing at all is `false`.
///
/// This is how "the deliverable contains this file" is decided: against the
/// immutable commit, not against a working directory that also holds whatever
/// git ignores. Read-only, through [`git_command`], with the path taken
/// literally (no glob, no pathspec magic).
///
/// `Err` for a `commit` that is not a full object id, a `path` that is not a
/// plain relative path, or a git failure — never a silent `false`.
pub fn commit_has_regular_file(repo_root: &Path, commit: &str, path: &str) -> Result<bool> {
    validate_object_id(commit)?;
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return Err(eyre!("`{path}` is not a plain repository-relative path")),
        }
    }
    let wanted = parts.join("/");
    if wanted.is_empty() || wanted.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(eyre!("`{path}` is not a plain repository-relative path"));
    }
    let output = git_command(repo_root)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args(["ls-tree", "-z", "--full-tree", commit, "--", &wanted])
        .output()
        .wrap_err("failed to run git ls-tree")?;
    if !output.status.success() {
        return Err(eyre!(
            "git ls-tree {commit} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // Each entry is `<mode> <type> <object>\t<path>`, NUL-terminated.
    let listing = String::from_utf8_lossy(&output.stdout);
    Ok(listing.split('\0').any(|entry| {
        entry.split_once('\t').is_some_and(|(meta, listed)| {
            listed == wanted && meta.starts_with("100") && meta.split(' ').nth(1) == Some("blob")
        })
    }))
}

/// `<stem>.<suffix>`: a directory that belongs to the task whose stem is
/// `stem` (`<work_root>/<fleet>/<task>`).
fn task_dir(stem: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = stem
        .file_name()
        .ok_or_else(|| eyre!("task directory {} has no name", stem.display()))?
        .to_os_string();
    name.push(".");
    name.push(suffix);
    Ok(stem.with_file_name(name))
}

/// A directory for a task's work checkout that no attempt has used before:
/// `<stem>.work-<random>`.
///
/// Every attempt works in a directory of its own. A process an interrupted
/// attempt left running knows only that attempt's directory (and its sandbox
/// confines it there), so it cannot write where the next attempt works.
pub fn new_work_checkout(stem: &Path) -> Result<PathBuf> {
    task_dir(stem, &format!("work-{}", uuid::Uuid::new_v4().simple()))
}

/// Whether `checkout` is a work checkout of the task whose stem is `stem` — a
/// sibling named `<stem>.work-…`. A path read back from a record is checked
/// with this before anything is done to it.
pub fn is_work_checkout(stem: &Path, checkout: &Path) -> bool {
    let (Some(stem_name), Some(name)) = (stem.file_name(), checkout.file_name()) else {
        return false;
    };
    checkout.parent() == stem.parent()
        && name
            .to_string_lossy()
            .strip_prefix(&*stem_name.to_string_lossy())
            .is_some_and(|rest| rest.starts_with(".work-") && rest.len() > ".work-".len())
}

/// The directory of a clean checkout for one verification of the task whose
/// stem is `stem`: `<stem>.verify-<label>`. See [`prepare_clean_checkout_dir`].
pub fn clean_checkout_path(stem: &Path, label: &str) -> Result<PathBuf> {
    task_dir(stem, &format!("verify-{label}"))
}

/// Remove every checkout directory that belongs to ONE task, except `keep`.
///
/// A task's directories all sit next to each other and are named after its
/// `stem` (`<work_root>/<fleet>/<task>`) plus a dot-suffix:
/// `<stem>.<anything>` ([`new_work_checkout`], [`clean_checkout_path`]). The
/// stem's own name never contains a dot (it comes from
/// [`crate::safe_filename`]), so the dot-suffix can only match THIS task's
/// directories — never another task whose id merely starts the same way, and
/// never the bare `<stem>` directory itself.
///
/// Each one is freed with [`remove_checkout_keep_branch`] (contained removal,
/// its own admin entry cleared, no branch deleted). Best-effort.
pub fn sweep_task_checkouts(repo_root: &Path, work_root: &Path, stem: &Path, keep: Option<&Path>) {
    let (Some(parent), Some(name)) = (stem.parent(), stem.file_name()) else {
        return;
    };
    let prefix = format!("{}.", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        if keep == Some(path.as_path()) {
            continue;
        }
        remove_checkout_keep_branch(repo_root, work_root, &path);
    }
}

/// A full git object id: 40 (SHA-1) or 64 (SHA-256) lower-case hex digits.
/// Anything else — a ref name, an abbreviation, an option — is refused, so a
/// value read from a record can never be parsed by git as something else.
fn validate_object_id(id: &str) -> Result<()> {
    let hex = id
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex && (id.len() == 40 || id.len() == 64) {
        Ok(())
    } else {
        Err(eyre!("`{id}` is not a full git object id"))
    }
}

/// The commit `branch` points at and the git tree id OF THAT COMMIT, read
/// host-side. The tree id is the digest a test receipt is bound to: it names the
/// exact content that was verified, independent of the mutable branch name.
///
/// The tree is resolved from the commit id just read — an immutable object —
/// never from the ref a second time, so a writer that moves the branch between
/// the two reads cannot pair one commit with another commit's tree.
///
/// Read-only (`rev-parse`) through [`git_command`], so it is safe to run
/// host-side on a branch a worker controls. A missing branch is an `Err`: the
/// caller must never treat "could not read" as an empty or matching digest.
pub fn branch_commit_and_tree(repo_root: &Path, branch: &str) -> Result<(String, String)> {
    validate_fleet_branch(branch)?;
    let commit = run_git(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ],
    )
    .wrap_err_with(|| format!("reading the head commit of {branch} failed"))?;
    let tree = run_git(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{commit}^{{tree}}"),
        ],
    )
    .wrap_err_with(|| format!("reading the tree of commit {commit} failed"))?;
    if commit.is_empty() || tree.is_empty() {
        return Err(eyre!(
            "git returned an empty commit or tree id for branch {branch}"
        ));
    }
    Ok((commit, tree))
}

/// The POSIX `sh -c` command an attempt runs — in a sandbox confined to the
/// interrupted attempt's checkout — to commit whatever that attempt left there,
/// so the leftover work becomes an immutable commit that can be digested and
/// verified.
///
/// It is [`deliverable_commit_command`] preceded by the removal of a stale
/// `index.lock`: a host killed during a git index write leaves that lock behind,
/// and every later `git add` in the checkout would refuse to run. Removing it is
/// sound only because reclamation happens after the prior attempt's lease was
/// reclaimed; it does NOT stop a writer that outlived its host. The caller
/// deals with that one by not staying: it retires the checkout once this
/// command has committed, and continues in another directory.
///
/// The caller gates on this command's exit code: a snapshot that did not commit
/// leaves the tree unknown, and an unknown tree cannot be verified. A directory
/// that holds exactly the branch's tree is still a no-op (exit 0).
///
/// `Err` for a `branch` that is not a valid fleet branch name.
///
/// PLATFORM: POSIX `sh`, like [`deliverable_commit_command`].
pub fn recovery_snapshot_command(branch: &str, message: &str) -> Result<String> {
    Ok(format!(
        "rm -f \"$(git rev-parse --git-dir)/index.lock\" && {}",
        deliverable_commit_command(branch, message)?
    ))
}

/// Parse a linked worktree's `<checkout>/.git` pointer file
/// (`gitdir: <git_dir>/worktrees/<slug>`) to locate its per-worktree admin dir.
fn worktree_admin_dir(checkout: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(checkout.join(".git")).ok()?;
    let target = content.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(target);
    Some(if path.is_absolute() {
        path
    } else {
        checkout.join(path)
    })
}

/// The POSIX `sh -c` command a fleet worktree worker's auto-commit runs INSIDE
/// its OWN sandbox (dissolution (b), module SECURITY note) to land what the
/// checkout it runs in HOLDS on the task branch `branch`: `git add -A`, then —
/// only if that content is not already the branch's tree — one new commit of
/// it whose parent is the branch's head, and the branch moved to that commit.
/// A directory that holds exactly the branch's tree is a no-op (exit 0), so it
/// is safe to run even when the worker already committed.
///
/// The commit is built from the directory's content and the BRANCH, not from
/// the checkout's `HEAD` (`write-tree`, `commit-tree`, `update-ref`). A worker
/// that detached its `HEAD` or switched to another branch still delivers what
/// its directory holds, on the task branch, and nothing is committed to
/// whatever `HEAD` names. With `HEAD` on the task branch — the ordinary case —
/// the result is the commit `git commit` would have made. The branch moves
/// only if it still points where this command read it; a branch something else
/// moved in between fails the command (non-zero exit) rather than being
/// overwritten.
///
/// It runs at the WORKER's grant (network `None` → no egress), so a
/// worker-planted local-config `filter.*.clean` triggered by `git add` is
/// CONTAINED — never executed host-side with the controller's network. Hooks
/// are disabled (`-c core.hooksPath=/dev/null`, belt), and `commit-tree` runs
/// none. The repository's LOCAL config is left in force: running the worker's
/// OWN local-config `filter.*` at its contained grant is exactly the point
/// (dissolution (b)). The string itself does not mask the global/system config
/// either; the fleet worker's command builder does that in the environment, as
/// the shell tool does for the agent's own `git` commands. Whether a
/// deliverable landed is decided by the caller from the branch's commit and
/// tree, read host-side ([`branch_commit_and_tree`]).
///
/// `Err` for a `branch` that is not a valid fleet branch name.
///
/// PLATFORM: the returned string is POSIX `sh` (`&&`, `$(…)`, `if …; then …;
/// fi`, single-quote escaping) and is only valid where the worker sandbox runs
/// a POSIX shell. `cmd /C` — the Windows shell fallback — cannot execute it, so
/// fleet worktree WORKERS are Unix-only. The controller-side helpers in this
/// module (which spawn git directly, no shell) are cross-platform, and their
/// tests run everywhere; the tests that exercise THIS contract by shelling out
/// to `sh` are `#[cfg(unix)]`.
pub fn deliverable_commit_command(branch: &str, message: &str) -> Result<String> {
    validate_fleet_branch(branch)?;
    let msg = sh_single_quote(message);
    let branch_ref = sh_single_quote(&format!("refs/heads/{branch}"));
    // `-c core.hooksPath=/dev/null` on the ops that could run a hook (belt):
    // even a planted `post-index-change` or `reference-transaction` hook never
    // runs. This is defense in depth; the commit already runs INSIDE the
    // worker's sandbox at its (contained) grant. The LOCAL config is
    // deliberately left in force: the worker's OWN local-config `filter.*`
    // running at its contained grant is the point (dissolution (b)).
    Ok(format!(
        "git -c core.hooksPath=/dev/null add -A \
         && tree=$(git write-tree) \
         && tip=$(git rev-parse --verify --quiet {branch_ref}) \
         && if [ \"$tree\" != \"$(git rev-parse --verify --quiet \"${{tip}}^{{tree}}\")\" ]; then \
         commit=$(git -c user.name='octos fleet worker' -c user.email='fleet-worker@octos.local' \
         -c commit.gpgsign=false commit-tree \"$tree\" -p \"$tip\" -m {msg}) \
         && git -c core.hooksPath=/dev/null update-ref {branch_ref} \"$commit\" \"$tip\"; fi"
    ))
}

/// The POSIX `sh -c` command that POPULATES a fleet worktree's working tree
/// INSIDE the worker's own sandbox. [`prepare_fleet_worktree`] creates the
/// checkout with `--no-checkout` (so a planted local smudge filter can't run
/// host-side as the daemon — module SECURITY note), which leaves the working
/// tree EMPTY; the worker must therefore restore it from the branch head before
/// working. `git reset --hard` writes the working tree + index to HEAD (the
/// task branch), running any smudge filter at the WORKER's grant — contained.
/// Runs hooks-disabled (`-c core.hooksPath=/dev/null`); a fresh branch restores
/// the base tree, a resumed branch restores the prior attempt's committed work.
pub fn worktree_populate_command() -> String {
    "git -c core.hooksPath=/dev/null reset -q --hard".to_string()
}

/// Single-quote `s` for safe interpolation into a POSIX `sh -c` command (wrap in
/// `'…'`, render each embedded `'` as `'\''`). Used only for the deliverable
/// commit MESSAGE, which runs inside the WORKER's own sandbox — this is
/// correctness (a message with a quote must not break the command), not a trust
/// boundary.
fn sh_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Best-effort removal of a fleet task's worktree CHECKOUT, KEEPING the branch.
///
/// Used on two paths: (a) after a task is ACCEPTED (free the disk, keep the
/// `fleet/*` branch as the deliverable) and (b) to roll back a checkout a
/// dispatch created but never ran an attempt in. An interrupted or unaccepted
/// attempt's checkout is NOT removed — it may hold uncommitted work. Robust to
/// a LOCKED checkout (HIGH #5) and NEVER removes a
/// directory not proven contained in `fleet-work` (HIGH #2). Deliberately does
/// NOT run `git branch -D` — the branch is the deliverable the keeper/user
/// reviews and merges. Every git op runs hooks-disabled (module SECURITY note).
pub fn remove_checkout_keep_branch(repo_root: &Path, work_root: &Path, checkout: &Path) {
    // Compute the git common dir best-effort so a stale (possibly locked) admin
    // entry can be cleared. If it fails, everything else still runs — only the
    // admin-entry clearing is skipped (there is no safe repo-global substitute;
    // see `force_free_checkout`).
    let git_dir = git_common_dir(repo_root).ok();
    force_free_checkout(repo_root, git_dir.as_deref(), work_root, checkout);
}

/// Free a (possibly LOCKED) worktree checkout, KEEPING the branch (HIGH #5):
/// `unlock` (locked worktrees refuse a single `--force`), then
/// `remove --force --force`, then a CONTAINED (#2) dir cleanup for a leftover,
/// then clear any stale admin entry left behind (a locked entry survives, and
/// would keep the branch recorded as checked-out, wedging every relaunch).
/// All git ops run hooks-disabled.
///
/// Every step is scoped to THIS `checkout`. There is deliberately no
/// `git worktree prune`: prune is repo-GLOBAL and deletes the admin entry of
/// every worktree whose checkout is momentarily missing, and `git worktree add`
/// registers its entry BEFORE the checkout exists — so a prune here silently
/// unregistered any worktree being created concurrently, leaving its branch
/// checked out nowhere. That reaches across FEATURES (fleet checkouts and peer
/// fences share one repo), and `clear_stale_admin_entry` already covers this
/// checkout's own stale entry, which is all prune was wanted for.
///
/// `git_dir` is `None` only when the common dir could not be resolved; the
/// admin-entry clearing is then skipped rather than widened to a global prune.
fn force_free_checkout(repo: &Path, git_dir: Option<&Path>, work_root: &Path, checkout: &Path) {
    let checkout_str = checkout.to_string_lossy();
    // A locked worktree needs an explicit unlock (or `--force` TWICE). Both are
    // best-effort — errors ("not locked", "not a working tree") are expected.
    let _ = run_git(repo, &["worktree", "unlock", &checkout_str]);
    let _ = run_git(
        repo,
        &["worktree", "remove", "--force", "--force", &checkout_str],
    );
    // Clear a leftover checkout dir ONLY if provably contained in fleet-work.
    remove_contained_dir(work_root, checkout);
    // Clear a stale (possibly locked) admin entry still pointing at this
    // checkout, so git stops treating the directory as a worktree of the
    // task's branch.
    if let Some(git_dir) = git_dir {
        clear_stale_admin_entry(git_dir, checkout);
    }
}

/// Clear the worktree admin entry for `checkout` and NOTHING else.
///
/// The safe replacement for a repo-global `git worktree prune` on any cleanup
/// path: prune also unregisters worktrees that merely have no checkout on disk
/// yet, which is the normal state mid-`git worktree add`. See
/// [`force_free_checkout`]. Best-effort — a repo whose common dir cannot be
/// resolved is left untouched.
pub fn clear_worktree_admin_entry(repo_root: &Path, checkout: &Path) {
    if let Ok(git_dir) = git_common_dir(repo_root) {
        clear_stale_admin_entry(&git_dir, checkout);
    }
}

/// `remove_dir_all(checkout)` ONLY when it is proven contained in `fleet-work`
/// with no symlinked component (HIGH #2). A path we cannot prove contained is
/// left untouched and logged — we NEVER remove outside `fleet-work`.
fn remove_contained_dir(work_root: &Path, checkout: &Path) {
    if !checkout.exists() {
        return;
    }
    match assert_checkout_contained(work_root, checkout) {
        Ok(()) => {
            if let Err(error) = std::fs::remove_dir_all(checkout) {
                tracing::warn!(
                    checkout = %checkout.display(), %error,
                    "fleet worktree: failed to remove leftover checkout directory",
                );
            }
        }
        Err(error) => tracing::warn!(
            checkout = %checkout.display(), %error,
            "fleet worktree: refusing to remove a checkout not proven contained in fleet-work",
        ),
    }
}

/// Remove a stale worktree admin entry (`<git_dir>/worktrees/<slug>`) still
/// referencing `checkout` — the leftover a LOCKED worktree's `prune` skips,
/// which keeps the branch recorded as checked-out and wedges relaunch (HIGH #5).
/// Contained-path checked: only an entry physically under `<git_dir>/worktrees`
/// is removed, and only when its recorded `gitdir` resolves to `checkout`.
fn clear_stale_admin_entry(git_dir: &Path, checkout: &Path) {
    let worktrees_dir = git_dir.join("worktrees");
    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        return;
    };
    let want = canonical_leaf(checkout);
    for entry in entries.flatten() {
        let admin = entry.path();
        // Containment: never touch anything outside `<git_dir>/worktrees`.
        if !admin.starts_with(&worktrees_dir) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        // `gitdir` records "<checkout>/.git"; its parent is the checkout.
        let recorded = PathBuf::from(content.trim());
        let recorded_checkout = recorded.parent().map(Path::to_path_buf).unwrap_or(recorded);
        if canonical_leaf(&recorded_checkout) == want && want.is_some() {
            if let Err(error) = std::fs::remove_dir_all(&admin) {
                tracing::warn!(
                    admin = %admin.display(), %error,
                    "fleet worktree: failed to clear stale worktree admin entry",
                );
            }
        }
    }
}

/// Resolve a path's canonical LOCATION even when its leaf does not exist:
/// canonicalize the parent (resolving any symlink) and re-attach the leaf. Used
/// to match a removed checkout against a recorded (canonical) `gitdir`.
fn canonical_leaf(p: &Path) -> Option<PathBuf> {
    let parent = p.parent()?;
    let name = p.file_name()?;
    Some(std::fs::canonicalize(parent).ok()?.join(name))
}

/// Prove `checkout` is safely contained in `work_root` (HIGH #2), rejecting any
/// symlinked component in its parent chain and any escape. `work_root` (the
/// pool's `fleet-work` root) is created + canonicalized as the trust anchor;
/// `checkout` must be LEXICALLY under it with only `Normal` components (the pool
/// derives it from `safe_filename`, so this is defence in depth), and every
/// EXISTING component from the root down must not be a symlink.
///
/// Point-in-time only (TOCTOU): a component could be swapped for a symlink
/// between this check and the subsequent git/`remove_dir_all`. That is sound
/// under v1's single-writer-per-task assumption (the pool also serialises
/// dispatch per task), a point-in-time containment, not a race-proof guarantee
/// against a hostile concurrent mutator of the task's own work root.
fn assert_checkout_contained(work_root: &Path, checkout: &Path) -> Result<()> {
    std::fs::create_dir_all(work_root)
        .wrap_err_with(|| format!("failed to create fleet-work root {}", work_root.display()))?;
    let canonical_root = std::fs::canonicalize(work_root).wrap_err_with(|| {
        format!(
            "failed to canonicalize fleet-work root {}",
            work_root.display()
        )
    })?;
    let rel = checkout.strip_prefix(work_root).map_err(|_| {
        eyre!(
            "worktree checkout {} is not under the fleet-work root {}",
            checkout.display(),
            work_root.display()
        )
    })?;
    let mut resolved = canonical_root.clone();
    for comp in rel.components() {
        match comp {
            Component::Normal(name) => {
                resolved.push(name);
                if let Ok(meta) = resolved.symlink_metadata() {
                    if meta.file_type().is_symlink() {
                        return Err(eyre!(
                            "worktree checkout component {} is a symlink; refusing to create or \
                             remove a worktree through it",
                            resolved.display()
                        ));
                    }
                }
            }
            Component::CurDir => {}
            _ => {
                return Err(eyre!(
                    "worktree checkout {} has an unsafe path component ({comp:?})",
                    checkout.display()
                ));
            }
        }
    }
    if !resolved.starts_with(&canonical_root) {
        return Err(eyre!(
            "worktree checkout {} resolves outside the fleet-work root {}",
            resolved.display(),
            canonical_root.display()
        ));
    }
    Ok(())
}

/// Set a local commit identity when the repo has NONE (HIGH #4). Never clobbers
/// an existing (global or local) identity; hooks-disabled like every op here.
fn provision_commit_identity(repo: &Path) {
    let has_email = run_git(repo, &["config", "user.email"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_email {
        let _ = run_git(repo, &["config", "user.email", "fleet-worker@octos.local"]);
        let _ = run_git(repo, &["config", "user.name", "octos fleet worker"]);
    }
}

/// The repository's common git directory (`<repo>/.git` for a normal repo),
/// absolute. This is what must be WRITABLE in the worker sandbox for a commit
/// inside a worktree checkout to succeed (objects/refs/logs and the new
/// worktree's admin files all live under it).
fn git_common_dir(repo_root: &Path) -> Result<PathBuf> {
    let raw = run_git(repo_root, &["rev-parse", "--git-common-dir"])?;
    let p = PathBuf::from(raw);
    let abs = if p.is_absolute() {
        p
    } else {
        repo_root.join(p)
    };
    Ok(std::fs::canonicalize(&abs).unwrap_or(abs))
}

/// Run a controller-side `git <args>` in `repo` through [`git_command`] (hooks
/// disabled + global/system config masked), returning trimmed stdout on success
/// or an error carrying stderr on failure. Routing EVERY op through
/// [`git_command`] is what closes the hook/filter code-exec surface across all
/// config sources (module SECURITY note).
fn run_git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = git_command(repo)
        .args(args)
        .output()
        .wrap_err_with(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        return Err(eyre!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Defensive validation of a fleet branch name. The pool derives it from
/// [`crate::safe_filename`] (only `[A-Za-z0-9_-]` and `%XX`), so this is a
/// belt-and-suspenders guard against argument injection or a traversal segment.
fn validate_fleet_branch(branch: &str) -> Result<()> {
    if branch.is_empty() || branch.len() > 200 {
        return Err(eyre!("fleet branch name has an invalid length"));
    }
    if branch.starts_with('-') {
        return Err(eyre!(
            "fleet branch name must not start with '-' (git argument injection)"
        ));
    }
    if branch
        .bytes()
        .any(|b| b < 0x20 || b == 0x7F || b == b' ' || b == b'\\' || b == b':')
    {
        return Err(eyre!(
            "fleet branch name contains control, space, or unsafe characters"
        ));
    }
    for segment in branch.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(eyre!("fleet branch name has an unsafe path segment"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Init a git repo in `dir` with one commit, so `HEAD` is valid for
    /// `worktree add -b … HEAD`. Skips (returns false) if git is unavailable.
    fn git_init_repo(dir: &Path) -> bool {
        if Command::new("git").arg("--version").output().is_err() {
            return false;
        }
        let ok = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        ok(&["init", "-q"])
            && ok(&["config", "user.email", "fleet@test"])
            && ok(&["config", "user.name", "fleet-test"])
            && ok(&["config", "commit.gpgsign", "false"])
            && {
                std::fs::write(dir.join("seed.txt"), b"seed\n").unwrap();
                ok(&["add", "-A"]) && ok(&["commit", "-q", "-m", "seed"])
            }
    }

    /// Shared fleet-worktree fixture: a git repo (skip when git is unavailable)
    /// plus a fresh work dir. Callers join their `<fleet>/<task>` checkout path
    /// onto the work dir; `None` keeps the historical skip-if-no-git semantics.
    fn fleet_fixture() -> Option<(tempfile::TempDir, tempfile::TempDir)> {
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return None;
        }
        let work = tempfile::tempdir().unwrap();
        Some((repo, work))
    }

    /// Simulate the worker's in-sandbox POPULATE step: `prepare_fleet_worktree`
    /// uses `--no-checkout`, so the working tree starts EMPTY and the worker must
    /// restore it (`git reset --hard`) before working. Every test that inspects
    /// or commits files in the checkout runs this first, as the real worker does.
    /// Unix: run the REAL command string through `sh`, exactly as the worker's
    /// sandbox does — the fidelity is the point.
    #[cfg(unix)]
    fn populate(checkout: &Path) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(worktree_populate_command())
            .current_dir(checkout)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Windows: `worktree_populate_command()` is POSIX `sh` and there is no
    /// shell to run it in, so perform the SAME git op natively. This is a test
    /// fixture (restore the `--no-checkout` working tree so `commit_in` has
    /// something to commit), not a claim that worktree workers run on Windows —
    /// it exists so the CONTROLLER-side tests, which are cross-platform, are not
    /// gated out by an incidental `sh` dependency.
    #[cfg(windows)]
    fn populate(checkout: &Path) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args(["-c", NO_HOOKS, "reset", "-q", "--hard"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn commit_in(checkout: &Path, name: &str) -> bool {
        let ok = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(checkout)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        // The worker populates the --no-checkout working tree before working.
        populate(checkout);
        std::fs::write(checkout.join(name), b"work\n").unwrap();
        ok(&["add", "-A"])
            && ok(&[
                "-c",
                "user.email=w@test",
                "-c",
                "user.name=w",
                "commit",
                "-q",
                "-m",
                "task work",
            ])
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings
    // (`populate`/`run_deliverable_command`), which `cmd /C` cannot run.
    #[cfg(unix)]
    #[test]
    fn prepare_reconciles_existing_branch_and_checkout_resuming_from_branch() {
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");

        // First attempt: create the worktree and commit into it.
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("first prepare");
        assert!(commit_in(&checkout, "out.txt"), "first commit");
        let branch_head_after_first = run_git(repo.path(), &["rev-parse", "fleet/f1/a"]).unwrap();

        // Simulate a dead attempt that never got cleaned up: the checkout dir AND
        // the branch are still present. A re-launch must reconcile without error…
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("re-prepare");
        assert!(checkout.is_dir(), "re-prepared checkout must exist");

        // …and RESUME from the branch (no -b, no branch reset): the branch head is
        // unchanged and its prior commit is still reachable.
        let branch_head_after_reprepare =
            run_git(&prepared.repo_root, &["rev-parse", "fleet/f1/a"]).unwrap();
        assert_eq!(
            branch_head_after_first, branch_head_after_reprepare,
            "re-prepare must resume from the branch, not reset it",
        );
        assert_eq!(
            prepared.base_commit, branch_head_after_first,
            "base for a resumed branch is its current head",
        );
        // The resumed checkout (populated in-sandbox, since re-prepare is
        // --no-checkout) has the prior attempt's file (restored from the branch).
        assert!(populate(&checkout), "worker populates the resumed worktree");
        assert!(
            checkout.join("out.txt").exists(),
            "resumed checkout must carry the prior attempt's committed work",
        );
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings
    // (`populate`/`run_deliverable_command`), which `cmd /C` cannot run.
    #[cfg(unix)]
    #[test]
    fn prepare_reclaims_a_locked_leftover_worktree() {
        // HIGH #5: a dead attempt left a LOCKED worktree. A single
        // `worktree remove --force` refuses a locked tree, and `prune` skips its
        // admin entry, wedging every relaunch. prepare must reclaim it.
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");

        // Create the leftover, then LOCK it (as an interrupted-but-locked attempt).
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("pre-create leftover");
        let lock = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "lock"])
            .arg(&checkout)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(lock, "should be able to lock the leftover");

        // Relaunch must reconcile the LOCKED leftover and re-add successfully.
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("relaunch must reclaim a locked leftover worktree");
        assert!(checkout.is_dir(), "reclaimed checkout must exist");
        assert!(
            git_ref_exists(&prepared.repo_root, "refs/heads/fleet/f1/a").unwrap(),
            "the fleet branch must survive the reclaim",
        );
        assert!(
            commit_in(&checkout, "out.txt"),
            "worker can commit after reclaim"
        );
    }

    /// Run [`deliverable_commit_command`] in `checkout` via `sh -c` (simulating
    /// the worker's sandbox running it). Returns whether it exited 0.
    #[cfg(unix)]
    fn run_deliverable_command(checkout: &Path, message: &str) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(deliverable_commit_command("fleet/f1/a", message).unwrap())
            .current_dir(checkout)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings
    // (`populate`/`run_deliverable_command`), which `cmd /C` cannot run.
    #[cfg(unix)]
    #[test]
    fn deliverable_commit_command_lands_changes_and_escapes_the_message() {
        // §4b: a worker wrote a file but did NOT commit. The auto-commit command
        // (run here via `sh -c`, standing in for the worker's sandbox) must land
        // it on the branch, advancing it. The commit MESSAGE is single-quote-escaped, so a message
        // containing a quote or shell metacharacters cannot break the command or
        // inject — it lands verbatim as the commit subject.
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        assert!(
            populate(&checkout),
            "worker populates the --no-checkout worktree"
        );

        // Worker writes a file WITHOUT committing; the plain-message commit must
        // land it and advance the branch.
        std::fs::write(checkout.join("out.txt"), b"deliverable\n").unwrap();
        assert!(
            run_deliverable_command(&checkout, "fleet a deliverable"),
            "the auto-commit command must succeed",
        );
        let head = run_git(&prepared.repo_root, &["rev-parse", "fleet/f1/a"]).unwrap();
        assert_ne!(
            head, prepared.base_commit,
            "an uncommitted change must land + advance the branch"
        );
        assert!(
            run_git(
                &prepared.repo_root,
                &["cat-file", "-e", "fleet/f1/a:out.txt"]
            )
            .is_ok(),
            "the file must be on the branch",
        );

        // A metacharacter message must not inject a command and must land
        // verbatim as the commit subject.
        std::fs::write(checkout.join("note.txt"), b"x\n").unwrap();
        let tricky = "fleet a's deliverable; touch PWNED $(id)";
        assert!(
            run_deliverable_command(&checkout, tricky),
            "the escaped command must run cleanly",
        );
        assert!(
            !checkout.join("PWNED").exists(),
            "a metacharacter message must not inject a command",
        );
        let subject = run_git(
            &prepared.repo_root,
            &["log", "-1", "--format=%s", "fleet/f1/a"],
        )
        .unwrap();
        assert_eq!(
            subject, tricky,
            "the message must land verbatim as the subject"
        );
    }

    /// Run [`recovery_snapshot_command`] in `checkout` via `sh -c` (standing in
    /// for the worker's sandbox). Returns whether it exited 0.
    #[cfg(unix)]
    fn run_recovery_snapshot(checkout: &Path, message: &str) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(recovery_snapshot_command("fleet/f1/a", message).unwrap())
            .current_dir(checkout)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // POSIX-sh contract: `commit_in` populates through the worker's `sh -c`
    // command string.
    #[cfg(unix)]
    #[test]
    fn should_return_commit_and_its_tree_when_branch_exists() {
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        let base_tree = run_git(&prepared.repo_root, &["rev-parse", "HEAD^{tree}"]).unwrap();

        // A fresh fleet branch points at the base commit and therefore its tree.
        let (commit, tree) =
            branch_commit_and_tree(&prepared.repo_root, "fleet/f1/a").expect("fresh branch");
        assert_eq!(commit, prepared.base_commit);
        assert_eq!(tree, base_tree);

        // After the worker commits, both move — and the tree is the tree OF the
        // returned commit, never of some other revision.
        assert!(commit_in(&checkout, "out.txt"), "worker commit");
        let (commit2, tree2) =
            branch_commit_and_tree(&prepared.repo_root, "fleet/f1/a").expect("advanced branch");
        assert_ne!(commit2, commit, "the branch advanced");
        assert_ne!(tree2, tree, "the tree changed with the new file");
        assert_eq!(
            tree2,
            run_git(
                &prepared.repo_root,
                &["rev-parse", &format!("{commit2}^{{tree}}")]
            )
            .unwrap(),
        );

        assert!(
            branch_commit_and_tree(&prepared.repo_root, "fleet/f1/missing").is_err(),
            "a missing branch is an error, never an empty digest",
        );
    }

    // POSIX-sh contract: populates through the worker's `sh -c` command string.
    #[cfg(unix)]
    #[test]
    fn should_find_a_recorded_checkout_recoverable_wherever_its_head_was_left() {
        // The host died after the worker wrote a patch but before anything was
        // committed. The directory is still a worktree git works in, so what it
        // holds can be committed — also when the worker had detached its HEAD,
        // or switched to a branch of its own, before it died.
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout).expect("prepare");
        assert!(populate(&checkout), "worker populates the worktree");
        std::fs::write(checkout.join("seed.txt"), b"patched\n").unwrap();
        std::fs::write(checkout.join("new.txt"), b"untracked work\n").unwrap();
        let inspect = || inspect_recorded_checkout(repo.path(), work.path(), &checkout).unwrap();

        assert_eq!(inspect(), RecordedCheckout::Recoverable, "on its branch");
        git_in(&checkout, &["checkout", "-q", "--detach"]);
        assert_eq!(inspect(), RecordedCheckout::Recoverable, "HEAD detached");
        git_in(&checkout, &["checkout", "-q", "-b", "elsewhere"]);
        assert_eq!(inspect(), RecordedCheckout::Recoverable, "other branch");

        assert_eq!(
            std::fs::read_to_string(checkout.join("seed.txt")).unwrap(),
            "patched\n",
            "looking at it changed nothing: the tracked edit is there",
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("new.txt")).unwrap(),
            "untracked work\n",
            "and so is the untracked file",
        );
    }

    #[test]
    fn should_tell_a_recorded_checkout_that_is_gone_from_one_git_cannot_commit_from() {
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");
        let inspect = || inspect_recorded_checkout(repo.path(), work.path(), &checkout).unwrap();

        // Nothing on disk, or a directory with nothing in it: nothing to lose.
        assert_eq!(inspect(), RecordedCheckout::Gone);
        std::fs::create_dir_all(&checkout).unwrap();
        assert_eq!(inspect(), RecordedCheckout::Gone);

        // Files in a directory git does not know as a worktree.
        std::fs::write(checkout.join("stray.txt"), b"x\n").unwrap();
        assert!(
            matches!(inspect(), RecordedCheckout::Unrecoverable(_)),
            "an unregistered directory that holds files",
        );
        std::fs::remove_dir_all(&checkout).unwrap();

        // A registered worktree that lost its `.git` file: git does not work
        // there any more, and the files in it are still somebody's work.
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout).expect("prepare");
        std::fs::write(checkout.join("work.txt"), b"work\n").unwrap();
        std::fs::remove_file(checkout.join(".git")).unwrap();
        assert!(
            matches!(inspect(), RecordedCheckout::Unrecoverable(_)),
            "a worktree without its `.git` file",
        );
        assert!(checkout.join("work.txt").exists(), "nothing was removed");
    }

    #[cfg(unix)]
    #[test]
    fn should_refuse_to_inspect_a_recorded_checkout_whose_path_is_a_symlink() {
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("f1")).unwrap();
        let checkout = work.path().join("f1").join("a");
        std::os::unix::fs::symlink(elsewhere.path(), &checkout).unwrap();

        assert!(
            inspect_recorded_checkout(repo.path(), work.path(), &checkout).is_err(),
            "a symlinked checkout must be refused, exactly as prepare refuses it",
        );
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings.
    #[cfg(unix)]
    #[test]
    fn should_commit_the_directory_to_the_task_branch_when_head_is_not_on_it() {
        // The worker detached its HEAD, committed there, and left more work
        // uncommitted. All of it is in the directory and none of it is on the
        // task branch. The snapshot puts the directory's content on the task
        // branch — and commits nothing to wherever HEAD points.
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        assert!(populate(&checkout), "worker populates the worktree");
        git_in(&checkout, &["checkout", "-q", "--detach"]);
        assert!(commit_in(&checkout, "committed.txt"), "a detached commit");
        std::fs::write(checkout.join("seed.txt"), b"patched\n").unwrap();
        std::fs::write(checkout.join("notes.txt"), b"untracked\n").unwrap();
        let detached = git_in(&checkout, &["rev-parse", "HEAD"]);

        assert!(run_recovery_snapshot(&checkout, "fleet a recovered work"));

        let (commit, _) = branch_commit_and_tree(&prepared.repo_root, "fleet/f1/a").unwrap();
        let on_branch = |path: &str| git_in(repo.path(), &["show", &format!("fleet/f1/a:{path}")]);
        assert_eq!(on_branch("seed.txt"), "patched", "the tracked edit");
        assert_eq!(on_branch("notes.txt"), "untracked", "the untracked file");
        assert_eq!(on_branch("committed.txt"), "work", "the detached commit");
        assert_eq!(
            git_in(repo.path(), &["rev-parse", "fleet/f1/a^"]),
            prepared.base_commit,
            "one commit on top of what the branch held",
        );
        assert_eq!(
            git_in(&checkout, &["rev-parse", "HEAD"]),
            detached,
            "HEAD itself was not moved",
        );

        // The same from a branch of the worker's own: that branch stays put.
        git_in(&checkout, &["checkout", "-q", "-b", "elsewhere"]);
        std::fs::write(checkout.join("seed.txt"), b"patched twice\n").unwrap();
        assert!(run_recovery_snapshot(&checkout, "fleet a recovered work"));
        assert_eq!(on_branch("seed.txt"), "patched twice");
        assert_eq!(git_in(repo.path(), &["rev-parse", "elsewhere"]), detached);
        assert_eq!(git_in(repo.path(), &["rev-parse", "fleet/f1/a^"]), commit);

        // Idempotent: an unchanged directory commits nothing.
        let (settled, _) = branch_commit_and_tree(&prepared.repo_root, "fleet/f1/a").unwrap();
        assert!(run_recovery_snapshot(&checkout, "fleet a recovered work"));
        assert_eq!(
            git_in(repo.path(), &["rev-parse", "fleet/f1/a"]),
            settled,
            "nothing new to commit",
        );
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings.
    #[cfg(unix)]
    #[test]
    fn should_snapshot_leftover_work_when_dead_writer_left_an_index_lock() {
        // A host killed in the middle of a git index write leaves `index.lock`
        // behind. The recovery snapshot must still land the leftover work.
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        assert!(populate(&checkout), "worker populates the worktree");
        std::fs::write(checkout.join("seed.txt"), b"patched\n").unwrap();
        std::fs::write(prepared.worktree_git_dir.join("index.lock"), b"").unwrap();
        let (_, base_tree) = branch_commit_and_tree(&prepared.repo_root, "fleet/f1/a").unwrap();

        assert!(
            run_recovery_snapshot(&checkout, "fleet a recovered work"),
            "the recovery snapshot must clear the stale lock and commit",
        );

        let (commit, tree) = branch_commit_and_tree(&prepared.repo_root, "fleet/f1/a").unwrap();
        assert_ne!(
            commit, prepared.base_commit,
            "the leftover work was committed"
        );
        assert_ne!(tree, base_tree, "the recovered tree differs from the base");
        assert_eq!(
            run_git(&prepared.repo_root, &["show", "fleet/f1/a:seed.txt"]).unwrap(),
            "patched",
        );

        // Idempotent: a second snapshot of an unchanged tree commits nothing.
        assert!(run_recovery_snapshot(&checkout, "fleet a recovered work"));
        let (commit2, tree2) = branch_commit_and_tree(&prepared.repo_root, "fleet/f1/a").unwrap();
        assert_eq!((commit2, tree2), (commit, tree));
    }

    #[test]
    fn controller_git_ops_do_not_fire_repo_hooks() {
        // CRITICAL #1B: a repo with a `post-checkout` hook that writes a marker.
        // The controller's `worktree add` (a checkout) must NOT fire it, because
        // every controller git op runs `-c core.hooksPath=/dev/null`.
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let marker = repo.path().join("HOOK_FIRED");
        let hook = repo.path().join(".git").join("hooks").join("post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let work = tempfile::tempdir().unwrap();
        let checkout = work.path().join("f1").join("a");
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout).expect("prepare");
        assert!(
            !marker.exists(),
            "the controller's worktree add must NOT fire the repo's post-checkout hook",
        );
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings
    // (`populate`/`run_deliverable_command`), which `cmd /C` cannot run.
    #[cfg(unix)]
    #[test]
    fn global_filter_is_neutralized_on_controller_ops() {
        // CRITICAL (codex re-review, fix 3): a `filter.*.clean` defined in GLOBAL
        // config (a worker with a writable HOME could plant one) runs on
        // `git add` — UNLESS the op masks GLOBAL config. Prove both directions
        // via `Command::env` (process env is left untouched — edition-2024-safe):
        // the CONTROL (global visible) runs the filter; the NEUTRALIZED op (via
        // `git_command`, GIT_CONFIG_GLOBAL=/dev/null) does NOT.
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let checkout = work.path().join("f1").join("a");
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout).expect("prepare");

        let evil = work.path().join("evil-gitconfig");
        let marker = work.path().join("FILTER_RAN");
        std::fs::write(
            &evil,
            format!(
                "[filter \"pwn\"]\n\tclean = sh -c 'touch {}'\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::write(checkout.join(".gitattributes"), b"out.txt filter=pwn\n").unwrap();
        std::fs::write(checkout.join("out.txt"), b"data\n").unwrap();

        // CONTROL: `git add` with the evil GLOBAL visible → the clean filter RUNS.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .env("GIT_CONFIG_GLOBAL", &evil)
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["add", "out.txt"])
            .status();
        let control_ran = marker.exists();
        let _ = std::fs::remove_file(&marker);
        // Reset the index so the neutralized add re-runs the clean path.
        let _ = run_git(&checkout, &["reset", "-q"]);

        // NEUTRALIZED: `git add` via git_command (GIT_CONFIG_GLOBAL=/dev/null) →
        // the filter is UNDEFINED there, so it must NOT run.
        let _ = run_git(&checkout, &["add", "out.txt"]);
        assert!(
            !marker.exists(),
            "a GLOBAL-defined filter must NOT run under git_command's GIT_CONFIG_GLOBAL=/dev/null",
        );
        assert!(
            control_ran,
            "sanity: the control (global visible) must run the filter, else the test is vacuous",
        );
    }

    /// Freeing ONE checkout must not unregister a SIBLING worktree.
    ///
    /// `remove_checkout_keep_branch` used to run a repo-GLOBAL
    /// `git worktree prune`, which deletes the admin entry of EVERY worktree
    /// whose checkout is currently missing — not just the one being freed. And
    /// `git worktree add` registers the admin entry BEFORE the checkout is
    /// populated, so any worktree being created concurrently sits in exactly
    /// that window. The victim is left with its branch checked out nowhere.
    ///
    /// This is cross-FEATURE, not just cross-task: fleet checkouts
    /// (`work_root/<fleet>/<task>`) and peer fences (`peers/<slug>/wt`) live in
    /// the SAME repo, so a fleet cleanup could unregister a peer's in-flight
    /// worktree. The targeted `clear_stale_admin_entry` already handles this
    /// checkout's own stale entry, so the global prune was pure blast radius.
    ///
    /// The race is a window, but its consequence is deterministic: a sibling
    /// whose checkout is absent must survive. Hiding it stands in for
    /// "mid-`worktree add`".
    #[test]
    fn removing_one_checkout_must_not_unregister_a_sibling_worktree() {
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };

        // The SURVIVOR and the one being freed — distinct paths, same repo.
        let sibling = work.path().join("f1").join("keeper");
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/keeper", &sibling)
            .expect("prepare sibling");
        let doomed = work.path().join("f1").join("doomed");
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/doomed", &doomed)
            .expect("prepare doomed");

        // Stand in for "the sibling is mid-`worktree add`": its admin entry
        // exists, its checkout is not on disk yet.
        let parked = work.path().join("keeper-parked");
        std::fs::rename(&sibling, &parked).unwrap();

        remove_checkout_keep_branch(repo.path(), work.path(), &doomed);

        std::fs::rename(&parked, &sibling).unwrap();

        // The freed checkout is gone…
        let listed = run_git(repo.path(), &["worktree", "list"]).unwrap_or_default();
        assert!(
            !listed.contains("doomed"),
            "the freed checkout must be unregistered, got:\n{listed}"
        );

        // …and the SIBLING is still a live worktree. Checking `git worktree
        // list` alone is not enough: a pruned entry leaves the directory on
        // disk, so resolve HEAD from INSIDE it, which only works while the
        // admin entry survives.
        let head = run_git(&sibling, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            head.unwrap_or_default(),
            "fleet/f1/keeper",
            "a sibling worktree must survive another checkout being freed; \
             worktree list was:\n{listed}"
        );
    }

    // Whole body is unix-only (needs `symlink`), like its sibling below —
    // gating the test itself keeps the Windows build warning-free.
    #[cfg(unix)]
    #[test]
    fn prepare_rejects_symlinked_checkout() {
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let parent = work.path().join("f1");
        std::fs::create_dir_all(&parent).unwrap();
        let checkout = parent.join("a");
        std::os::unix::fs::symlink(work.path(), &checkout).unwrap();
        assert!(
            prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout).is_err(),
            "a symlinked checkout path must be refused",
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepare_rejects_symlinked_parent() {
        // HIGH #2: a symlinked PARENT (not just the leaf) must be refused — the
        // old leaf-only check followed it, and a reconcile remove could then
        // delete OUTSIDE fleet-work.
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let outside = tempfile::tempdir().unwrap();
        // work/f1 -> <outside> (a symlinked parent of the checkout).
        std::os::unix::fs::symlink(outside.path(), work.path().join("f1")).unwrap();
        let checkout = work.path().join("f1").join("a");
        assert!(
            prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout).is_err(),
            "a symlinked PARENT component must be refused (no create/remove through it)",
        );
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
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

    /// Fill the empty directory `dir` with [`clean_checkout_command`] via
    /// `sh -c`, with the global and system configuration masked as the
    /// worker's command builder masks them. Returns whether it exited 0.
    #[cfg(unix)]
    fn run_clean_checkout(repo: &Path, dir: &Path, commit: &str) -> bool {
        let git_dir = git_common_dir(repo).unwrap();
        Command::new("sh")
            .arg("-c")
            .arg(clean_checkout_command(&git_dir, commit).unwrap())
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings.
    #[cfg(unix)]
    #[test]
    fn should_check_out_the_commits_own_bytes_when_the_repository_plants_a_filter() {
        use std::os::unix::fs::PermissionsExt;

        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let base = git_in(repo.path(), &["rev-parse", "HEAD"]);
        let task = work.path().join("f1").join("a");
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &task).expect("prepare");
        assert!(commit_in(&task, "out.txt"), "the task commits its work");
        let patched = git_in(repo.path(), &["rev-parse", "fleet/f1/a"]);

        // What a worker can write into the shared `.git`: a filter that
        // rewrites a file as it is checked out, the attribute line that applies
        // it, and a hook that runs on every checkout.
        let git_dir = git_common_dir(repo.path()).unwrap();
        let hook_ran = work.path().join("hook-ran");
        git_in(
            repo.path(),
            &["config", "filter.planted.smudge", "printf 'rewritten\\n'"],
        );
        std::fs::create_dir_all(git_dir.join("info")).unwrap();
        std::fs::write(git_dir.join("info/attributes"), "seed.txt filter=planted\n").unwrap();
        let hook = git_dir.join("hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", hook_ran.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Something already sits where the clean checkout will go: a directory
        // an earlier run left, with a cache file in it.
        let clean = work.path().join("f1").join("a.verify-1");
        std::fs::create_dir_all(&clean).unwrap();
        std::fs::write(clean.join("test-cache"), b"stale\n").unwrap();

        prepare_clean_checkout_dir(work.path(), &clean).expect("an empty directory");
        assert!(!clean.join("test-cache").exists(), "nothing left behind");
        assert!(run_clean_checkout(repo.path(), &clean, &base));

        assert_eq!(
            std::fs::read_to_string(clean.join("seed.txt")).unwrap(),
            "seed\n",
            "the commit's own bytes, not what the planted filter writes",
        );
        assert!(!hook_ran.exists(), "the planted hook did not run");
        assert!(
            !clean.join("out.txt").exists(),
            "nothing from a later commit"
        );
        assert_eq!(git_in(&clean, &["rev-parse", "HEAD"]), base);
        assert!(
            !Command::new("git")
                .arg("-C")
                .arg(&clean)
                .args(["symbolic-ref", "-q", "HEAD"])
                .output()
                .unwrap()
                .status
                .success(),
            "HEAD is detached: a commit made here moves no branch",
        );
        assert_eq!(
            git_in(repo.path(), &["rev-parse", "fleet/f1/a"]),
            patched,
            "the task branch is untouched",
        );

        // The filter is real: a worktree OF the task repository gets its bytes.
        std::fs::remove_file(task.join("seed.txt")).unwrap();
        assert!(populate(&task));
        assert_eq!(
            std::fs::read_to_string(task.join("seed.txt")).unwrap(),
            "rewritten\n",
            "a checkout that reads the repository's configuration is rewritten",
        );

        remove_checkout_keep_branch(repo.path(), work.path(), &clean);
        assert!(!clean.exists());
    }

    #[cfg(unix)]
    #[test]
    fn should_refuse_a_clean_checkout_command_it_cannot_write_safely() {
        let id = "a".repeat(40);
        for not_an_id in ["HEAD", "fleet/f1/a", "--force", "abc123", ""] {
            assert!(
                clean_checkout_command(Path::new("/repo/.git"), not_an_id).is_err(),
                "{not_an_id:?} must be refused",
            );
        }
        assert!(
            clean_checkout_command(Path::new("repo/.git"), &id).is_err(),
            "a relative object store would resolve against the new repository",
        );
        assert!(
            clean_checkout_command(Path::new("/re\npo/.git"), &id).is_err(),
            "an alternates file holds one path per line",
        );
        let quoted = clean_checkout_command(Path::new("/it's/.git"), &id).unwrap();
        assert!(
            quoted.contains(r"'/it'\''s/.git/objects'"),
            "the path is quoted for the shell: {quoted}",
        );
        let sha256 = clean_checkout_command(Path::new("/repo/.git"), &"b".repeat(64)).unwrap();
        assert!(sha256.contains("--object-format=sha256"), "got: {sha256}");
    }

    #[cfg(unix)]
    #[test]
    fn should_tell_a_regular_file_in_a_commit_from_everything_else() {
        let Some((repo, _work)) = fleet_fixture() else {
            return;
        };
        std::fs::create_dir_all(repo.path().join("docs")).unwrap();
        std::fs::write(repo.path().join("docs/report.md"), b"report\n").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", repo.path().join("link.txt")).unwrap();
        git_in(repo.path(), &["add", "-A"]);
        git_in(repo.path(), &["commit", "-q", "-m", "files"]);
        let commit = git_in(repo.path(), &["rev-parse", "HEAD"]);
        // Present on disk but not in the commit.
        std::fs::write(repo.path().join("untracked.txt"), b"x\n").unwrap();

        let has = |path: &str| commit_has_regular_file(repo.path(), &commit, path);
        assert!(has("seed.txt").unwrap());
        assert!(has("docs/report.md").unwrap());
        assert!(has("./docs/report.md").unwrap());
        assert!(!has("docs").unwrap(), "a directory is not a file");
        assert!(!has("link.txt").unwrap(), "a symlink is not a regular file");
        assert!(!has("missing.txt").unwrap());
        assert!(!has("untracked.txt").unwrap(), "only the commit counts");
        assert!(!has("*.txt").unwrap(), "the path is literal, not a glob");
        assert!(has("/etc/passwd").is_err());
        assert!(has("../seed.txt").is_err());
        assert!(has("").is_err());
        assert!(
            commit_has_regular_file(repo.path(), "HEAD", "seed.txt").is_err(),
            "a ref name is not an object id",
        );
    }

    #[test]
    fn should_name_every_directory_of_a_task_after_its_stem() {
        let stem = Path::new("/work/f1/a");
        let first = new_work_checkout(stem).unwrap();
        let second = new_work_checkout(stem).unwrap();
        assert_ne!(first, second, "no two work checkouts share a directory");
        assert_eq!(first.parent(), stem.parent());
        assert!(is_work_checkout(stem, &first) && is_work_checkout(stem, &second));

        let clean = clean_checkout_path(stem, "attempt-0").unwrap();
        assert_eq!(clean, Path::new("/work/f1/a.verify-attempt-0"));

        for not_a_work_checkout in [
            "/work/f1/a",
            "/work/f1/a.work-",
            "/work/f1/a.verify-attempt-0",
            "/work/f1/ab.work-1",
            "/work/f2/a.work-1",
            "/elsewhere/a.work-1",
        ] {
            assert!(
                !is_work_checkout(stem, Path::new(not_a_work_checkout)),
                "{not_a_work_checkout} is not a work checkout of task a",
            );
        }
    }

    // POSIX-sh contract: populates through the worker's `sh -c` command string.
    #[cfg(unix)]
    #[test]
    fn should_add_a_second_checkout_of_the_branch_when_an_earlier_one_still_holds_work() {
        // An interrupted attempt's directory still has the task branch checked
        // out, with a patch that was never committed. The next attempt's
        // directory is created beside it; nothing in the old one is touched.
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let stem = work.path().join("f1").join("a");
        let old = new_work_checkout(&stem).unwrap();
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &old).expect("first");
        assert!(populate(&old));
        std::fs::write(old.join("seed.txt"), b"patched\n").unwrap();

        let new = new_work_checkout(&stem).unwrap();
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &new)
            .expect("a second checkout of the same branch");

        assert_eq!(
            std::fs::read_to_string(old.join("seed.txt")).unwrap(),
            "patched\n",
            "the uncommitted patch is untouched",
        );
        assert_eq!(
            inspect_recorded_checkout(repo.path(), work.path(), &old).unwrap(),
            RecordedCheckout::Recoverable,
            "the old checkout is still a worktree its work can be committed from",
        );
        assert!(!new.join("seed.txt").exists(), "created empty");
        // Once the old work is committed and the old directory retired, the new
        // one is populated from the branch and holds that work.
        assert!(run_recovery_snapshot(&old, "recovered"));
        remove_checkout_keep_branch(repo.path(), work.path(), &old);
        assert!(populate(&new));
        assert_eq!(
            std::fs::read_to_string(new.join("seed.txt")).unwrap(),
            "patched\n",
        );
    }

    #[test]
    fn should_remove_only_this_tasks_leftover_checkouts_when_sweeping() {
        let Some((repo, work)) = fleet_fixture() else {
            return;
        };
        let fleet = work.path().join("f1");
        let stem = fleet.join("a");
        // A work checkout a dead attempt never ran in, and a clean checkout a
        // verification left behind.
        let leftover = fleet.join("a.work-1");
        let verified = fleet.join("a.verify-1");
        let kept = fleet.join("a.work-2");
        let other_task = fleet.join("ab.work-3");
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &leftover)
            .expect("leftover");
        prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/ab", &other_task)
            .expect("other");
        prepare_clean_checkout_dir(work.path(), &verified).expect("verified");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::create_dir_all(&stem).unwrap();

        sweep_task_checkouts(repo.path(), work.path(), &stem, Some(&kept));

        assert!(!leftover.exists(), "this task's leftover is removed");
        assert!(!verified.exists(), "and so is its clean checkout");
        assert!(kept.exists(), "the checkout to keep is kept");
        assert!(
            other_task.exists(),
            "another task's checkout is not touched"
        );
        assert!(stem.exists(), "the bare stem directory is not touched");
        let listing = git_in(repo.path(), &["worktree", "list", "--porcelain"]);
        assert!(!listing.contains("a.work-1"), "and unregistered: {listing}");
        assert!(listing.contains("ab.work-3"), "got: {listing}");
    }

    #[test]
    fn validate_fleet_branch_rejects_injection_and_traversal() {
        assert!(validate_fleet_branch("fleet/f1/a").is_ok());
        assert!(validate_fleet_branch("-rf").is_err());
        assert!(validate_fleet_branch("fleet/../evil").is_err());
        assert!(validate_fleet_branch("fleet/f 1/a").is_err());
        assert!(validate_fleet_branch("").is_err());
    }
}
