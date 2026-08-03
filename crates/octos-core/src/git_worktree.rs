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
    /// The branch's commit right after prepare — the BASE that
    /// [`branch_advanced_past`] compares against to decide whether the attempt
    /// actually landed a deliverable (branch advanced past this) or left an empty
    /// branch.
    pub base_commit: String,
}

/// Allocate — or, for a re-launched attempt, RECONCILE — the TASK-STABLE
/// worktree for a fleet task.
///
/// `work_root` is the pool's `fleet-work` root; the `checkout` MUST resolve
/// safely under it (no symlinked component, no escape — [`assert_checkout_contained`]),
/// which is what makes the reconcile's removals safe (HIGH #2: never create or
/// `remove_dir_all` outside `fleet-work`).
///
/// The branch (`fleet/<fleet>/<task>`) and checkout path are deterministic and
/// stable across attempts, which is what makes a restarted attempt resumable:
/// it continues from the dead attempt's last committed work. But it also means
/// a re-launch finds the branch and/or its checkout ALREADY PRESENT, where a
/// plain `git worktree add -b` would fail. This reconciles deterministically
/// (v1 = interrupt-and-restart, matching the kernel's "never resume a live
/// turn"):
///
/// 1. If the checkout dir or the branch exists, [`force_free_checkout`] frees a
///    leftover checkout — even a LOCKED one (HIGH #5: `unlock` +
///    `remove --force --force` + `prune` + clear a stale admin entry) — WITHOUT
///    deleting the branch, so the relaunch can always reclaim the task.
/// 2. Re-add: if the branch still exists, `git worktree add <checkout> <branch>`
///    (NO `-b`) so the checkout RESUMES from the branch's last commit;
///    otherwise `git worktree add -b <branch> <checkout> HEAD` for a fresh task.
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
            &["worktree", "add", "--no-checkout", &checkout_str, branch],
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
/// its OWN sandbox (dissolution (b), module SECURITY note) to land any
/// uncommitted deliverable on its branch: `git add -A` then, only if something
/// is staged, a headless `git commit` (provisioned identity, `--no-verify`, no
/// gpg-sign). A clean tree is a no-op (exit 0), so it is safe to run even when
/// the worker already committed.
///
/// It runs at the WORKER's grant (network `None` → no egress), so a
/// worker-planted local-config `filter.*.clean` triggered by `git add`, or a
/// commit hook, is CONTAINED — never executed host-side with the controller's
/// network. Hooks are disabled here too (`-c core.hooksPath=/dev/null`, belt) so
/// even a planted `post-commit` never runs; it deliberately does NOT mask
/// `GIT_CONFIG_GLOBAL`/`SYSTEM` like [`git_command`], because running the
/// worker's OWN local-config `filter.*` at its contained grant is exactly the
/// point (dissolution (b)). The caller does NOT gate on this command's exit
/// code (a clean tree, or a worker's own broken filter, exits non-zero without
/// being an infra error) — the authoritative "did a deliverable land?" check is
/// [`branch_advanced_past`], read host-side.
///
/// PLATFORM: the returned string is POSIX `sh` (`&&`, `if ! …; then …; fi`,
/// single-quote escaping) and is only valid where the worker sandbox runs a
/// POSIX shell. `cmd /C` — the Windows shell fallback — cannot execute it, so
/// fleet worktree WORKERS are Unix-only. The controller-side helpers in this
/// module (which spawn git directly, no shell) are cross-platform, and their
/// tests run everywhere; the tests that exercise THIS contract by shelling out
/// to `sh` are `#[cfg(unix)]`.
pub fn deliverable_commit_command(message: &str) -> String {
    let msg = sh_single_quote(message);
    // `-c core.hooksPath=/dev/null` on BOTH ops (belt): disables ALL git hooks so
    // even a planted `post-commit`/`post-index-change` hook — which `--no-verify`
    // does NOT skip — never runs. This is defense in depth; the commit already
    // runs INSIDE the worker's sandbox at its (contained) grant. It deliberately
    // does NOT mask `GIT_CONFIG_GLOBAL`/`SYSTEM`: the worker's OWN local-config
    // `filter.*` running at its contained grant is the point (dissolution (b)).
    format!(
        "git -c core.hooksPath=/dev/null add -A && if ! git diff --cached --quiet; then \
         git -c core.hooksPath=/dev/null \
         -c user.name='octos fleet worker' -c user.email='fleet-worker@octos.local' \
         -c commit.gpgsign=false commit --no-verify -q -m {msg}; fi"
    )
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

/// Whether `branch`'s head in `repo_root` has ADVANCED past `base_commit` — i.e.
/// the worktree attempt landed a real commit. Read-only (`rev-parse`), routed
/// through [`git_command`] (hooks-disabled + global/system-config masked), so it
/// is safe to run host-side; the MUTATING commit runs in the worker sandbox via
/// [`deliverable_commit_command`].
///
/// `Ok(false)` means the branch is unchanged — an accepted run that left an EMPTY
/// branch, which the caller must NOT record as a success. `Err` is an infra
/// failure reading the ref (the caller terminates rather than record a phantom).
pub fn branch_advanced_past(repo_root: &Path, branch: &str, base_commit: &str) -> Result<bool> {
    let head = run_git(repo_root, &["rev-parse", branch])
        .wrap_err("deliverable check: reading the fleet branch head failed")?;
    Ok(!head.is_empty() && head != base_commit)
}

/// Best-effort removal of a fleet task's worktree CHECKOUT, KEEPING the branch.
///
/// Used on two paths: (a) after an attempt completes (free the disk, keep the
/// `fleet/*` branch as the deliverable) and (b) when an attempt is interrupted
/// (so a dead attempt does not leave a locked checkout blocking the next
/// attempt's re-add). Robust to a LOCKED checkout (HIGH #5) and NEVER removes a
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
    // checkout, so a relaunch can reclaim the task-stable branch.
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

    #[test]
    fn is_git_repo_true_for_repo_false_for_plain_dir() {
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return; // git unavailable
        }
        assert!(is_git_repo(repo.path()));
        let plain = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(plain.path()));
    }

    #[test]
    fn probe_distinguishes_non_repo_from_probe_error() {
        // MEDIUM #7: a plain dir is a CONFIRMED non-repo (Ok(false) → scratch);
        // a repo is Ok(true); a FILE path (git: "cannot change to …: Not a
        // directory", NOT "not a git repository") is a probe ERROR (Err) — the
        // pool must propagate it, never silently scratch a real repo.
        let plain = tempfile::tempdir().unwrap();
        if Command::new("git").arg("--version").output().is_err() {
            return; // git unavailable
        }
        assert_eq!(probe_git_repo(plain.path()).ok(), Some(false));

        let repo = tempfile::tempdir().unwrap();
        if git_init_repo(repo.path()) {
            assert_eq!(probe_git_repo(repo.path()).ok(), Some(true));
        }

        // A file passed where a dir is expected → git errors with a non-"not a
        // git repository" message → probe error.
        let file = plain.path().join("afile");
        std::fs::write(&file, b"x").unwrap();
        assert!(
            probe_git_repo(&file).is_err(),
            "a git -C <file> failure must be a probe ERROR, not a confirmed non-repo",
        );

        // An absent root is a definite non-repo (scratch), not a probe error.
        assert_eq!(probe_git_repo(&plain.path().join("nope")).ok(), Some(false),);
    }

    #[test]
    fn prepare_creates_worktree_on_fleet_branch() {
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let checkout = work.path().join("f1").join("a");

        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare worktree");

        // The checkout is a real directory (the worker cwd).
        assert!(checkout.is_dir(), "checkout must be a real directory");
        // The branch was created in the repo.
        assert!(
            git_ref_exists(&prepared.repo_root, "refs/heads/fleet/f1/a").unwrap(),
            "fleet branch must exist after prepare",
        );
        // git_dir points at the repo's .git.
        assert!(
            prepared.git_dir.ends_with(".git"),
            "git_dir must be the repo .git, got {}",
            prepared.git_dir.display(),
        );
        // base_commit is the seed (branch just created off HEAD).
        let seed = run_git(&prepared.repo_root, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            prepared.base_commit, seed,
            "base is the branch's start commit"
        );
        // A commit inside the checkout lands on the fleet branch.
        assert!(commit_in(&checkout, "out.txt"), "commit in worktree");
        let branch_head = run_git(&prepared.repo_root, &["rev-parse", "fleet/f1/a"]).unwrap();
        assert_ne!(
            branch_head, seed,
            "the fleet branch must advance past the seed commit",
        );
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings
    // (`populate`/`run_deliverable_command`), which `cmd /C` cannot run.
    #[cfg(unix)]
    #[test]
    fn prepare_reconciles_existing_branch_and_checkout_resuming_from_branch() {
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
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
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
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
            .arg(deliverable_commit_command(message))
            .current_dir(checkout)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings
    // (`populate`/`run_deliverable_command`), which `cmd /C` cannot run.
    #[cfg(unix)]
    #[test]
    fn deliverable_commit_command_lands_uncommitted_changes() {
        // §4b: a worker wrote a file but did NOT commit. The auto-commit command
        // (run here via `sh -c`, standing in for the worker's sandbox) must land
        // it on the branch, and `branch_advanced_past` must report a real
        // deliverable.
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let checkout = work.path().join("f1").join("a");
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        assert!(
            populate(&checkout),
            "worker populates the --no-checkout worktree"
        );

        // Worker writes a file WITHOUT committing.
        std::fs::write(checkout.join("out.txt"), b"deliverable\n").unwrap();
        assert!(
            run_deliverable_command(&checkout, "fleet a deliverable"),
            "the auto-commit command must succeed",
        );
        let landed = branch_advanced_past(&prepared.repo_root, "fleet/f1/a", &prepared.base_commit)
            .expect("branch check");
        assert!(
            landed,
            "an uncommitted change must land + advance the branch"
        );
        let head = run_git(&prepared.repo_root, &["rev-parse", "fleet/f1/a"]).unwrap();
        assert_ne!(head, prepared.base_commit, "branch advanced");
        assert!(
            run_git(
                &prepared.repo_root,
                &["cat-file", "-e", "fleet/f1/a:out.txt"]
            )
            .is_ok(),
            "the file must be on the branch",
        );
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings
    // (`populate`/`run_deliverable_command`), which `cmd /C` cannot run.
    #[cfg(unix)]
    #[test]
    fn deliverable_command_noop_and_branch_empty_when_nothing_produced() {
        // §4b: a worker that produced nothing leaves the branch at base — the
        // command is a clean no-op (exit 0) on an unchanged tree and
        // `branch_advanced_past` reports `false` so the caller does not record a
        // phantom success.
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let checkout = work.path().join("f1").join("a");
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        // Populate the --no-checkout tree so the index matches HEAD; otherwise
        // the empty index would show the seed as a staged deletion and the
        // auto-commit would spuriously advance the branch.
        assert!(
            populate(&checkout),
            "worker populates the --no-checkout worktree"
        );

        assert!(
            run_deliverable_command(&checkout, "fleet a deliverable"),
            "the auto-commit command must exit 0 (clean no-op) on an unchanged tree",
        );
        let landed = branch_advanced_past(&prepared.repo_root, "fleet/f1/a", &prepared.base_commit)
            .expect("branch check");
        assert!(!landed, "no changes → no deliverable → branch unchanged");
        let head = run_git(&prepared.repo_root, &["rev-parse", "fleet/f1/a"]).unwrap();
        assert_eq!(head, prepared.base_commit, "branch stays at base");
    }

    // POSIX-sh contract: drives the worker's `sh -c` command strings
    // (`populate`/`run_deliverable_command`), which `cmd /C` cannot run.
    #[cfg(unix)]
    #[test]
    fn deliverable_commit_command_escapes_the_message() {
        // The commit MESSAGE is single-quote-escaped, so a message containing a
        // quote or shell metacharacters cannot break the command or inject — it
        // lands verbatim as the commit subject and no injected command runs.
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let checkout = work.path().join("f1").join("a");
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        assert!(
            populate(&checkout),
            "worker populates the --no-checkout worktree"
        );
        std::fs::write(checkout.join("out.txt"), b"x\n").unwrap();

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

    #[cfg(unix)]
    #[test]
    fn deliverable_commit_command_disables_hooks() {
        // Belt: the worker's own sandboxed commit runs `-c core.hooksPath=/dev/null`
        // so even a planted `post-commit` hook (which `--no-verify` does NOT skip)
        // never runs. It is contained at the worker's grant anyway; this is
        // defense in depth so no planted hook executes at all.
        use std::os::unix::fs::PermissionsExt;

        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let checkout = work.path().join("f1").join("a");
        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        assert!(populate(&checkout), "worker populates the worktree");

        // Plant a post-commit hook in the shared hooks dir that drops a marker.
        let hooks_dir = prepared.git_dir.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let marker = repo.path().join("HOOK_RAN");
        let hook = hooks_dir.join("post-commit");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        std::fs::write(checkout.join("out.txt"), b"deliverable\n").unwrap();
        assert!(
            run_deliverable_command(&checkout, "fleet a deliverable"),
            "the auto-commit must succeed",
        );
        // The commit landed...
        assert!(
            branch_advanced_past(&prepared.repo_root, "fleet/f1/a", &prepared.base_commit).unwrap(),
            "the deliverable must land on the branch",
        );
        // ...but the planted post-commit hook did NOT run.
        assert!(
            !marker.exists(),
            "a planted post-commit hook must NOT run — the commit is hooks-disabled",
        );
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

    #[test]
    fn git_command_neutralizes_all_config_sources() {
        // CRITICAL (codex re-review, fix 3): every controller git op must mask
        // GLOBAL + SYSTEM config to /dev/null AND disable hooks, so no
        // worker-influenced config source can define a `filter.*`/hook. Assert
        // the centralized builder wires all three neutralizers.
        let cmd = git_command(Path::new("/some/repo"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1] == NO_HOOKS),
            "must disable hooks, args: {args:?}",
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-c" && w[1] == "core.fsmonitor="),
            "must override fsmonitor to empty (belt), args: {args:?}",
        );
        let envs: std::collections::HashMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs.get("GIT_CONFIG_GLOBAL"),
            Some(&Some(NULL_DEVICE.to_string())),
            "must mask GLOBAL config to the null device",
        );
        assert_eq!(
            envs.get("GIT_CONFIG_SYSTEM"),
            Some(&Some(NULL_DEVICE.to_string())),
            "must mask SYSTEM config to the null device",
        );
    }

    #[test]
    fn controller_git_command_uses_absolute_binary() {
        // HIGH (controller-hijack): a full-FS worker could plant a fake `git`
        // earlier in the controller's `$PATH`. Every controller-side git op must
        // invoke an ABSOLUTE binary so `$PATH` is never consulted.
        let cmd = git_command(Path::new("/some/repo"));
        let prog = cmd.get_program();
        assert!(
            Path::new(prog).is_absolute(),
            "controller git must use an ABSOLUTE binary (no $PATH lookup), got {prog:?}",
        );
        assert!(
            GIT_BIN.is_absolute(),
            "GIT_BIN must be absolute, got {:?}",
            *GIT_BIN
        );
    }

    #[test]
    fn controller_git_command_env_is_sanitized() {
        // HIGH (controller-hijack): the controller git op must not inherit ANY
        // controller provider secret — heuristic (`OPENAI_API_KEY`) OR
        // runtime-REGISTERED but non-heuristic (`VERTEX_SA_JSON`). Re-exec THIS
        // test binary with both in its env; the child asserts `git_command` strips
        // both (the same set the worker-sandboxed ops strip) while KEEPING the git
        // config overrides.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("git_worktree::tests::child_controller_git_env_sanitized")
            .arg("--exact")
            .arg("--ignored")
            .env("OPENAI_API_KEY", "sk-controller-secret")
            .env("VERTEX_SA_JSON", "{\"private_key\":\"sa-json-secret\"}")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child regression failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    #[ignore]
    fn child_controller_git_env_sanitized() {
        // Runs in a re-exec'd process holding `OPENAI_API_KEY` + `VERTEX_SA_JSON`.
        // `VERTEX_SA_JSON` is a registered provider secret whose NAME the heuristic
        // does NOT flag, so register it (as `profile_factory` does in production)
        // before building the command.
        assert!(
            !crate::is_secret_env_name("VERTEX_SA_JSON"),
            "VERTEX_SA_JSON must be a name the heuristic does NOT flag",
        );
        crate::register_secret_env_names(["VERTEX_SA_JSON"]);

        let cmd = git_command(Path::new("/some/repo"));
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        // The heuristic provider key is STRIPPED.
        assert!(
            envs.iter()
                .any(|(k, v)| k == "OPENAI_API_KEY" && v.is_none()),
            "OPENAI_API_KEY must be stripped from the controller git env: {envs:?}",
        );
        // The REGISTERED (non-heuristic) provider secret is ALSO stripped — the
        // gap this fix closes.
        assert!(
            envs.iter()
                .any(|(k, v)| k == "VERTEX_SA_JSON" && v.is_none()),
            "registered VERTEX_SA_JSON must be stripped from the controller git env: {envs:?}",
        );
        // An injection var is stripped unconditionally.
        assert!(
            envs.iter().any(|(k, v)| k == "LD_PRELOAD" && v.is_none()),
            "LD_PRELOAD must be stripped: {envs:?}",
        );
        // The git-specific overrides REMAIN (config masking intact).
        assert!(
            envs.iter()
                .any(|(k, v)| k == "GIT_CONFIG_GLOBAL" && v.as_deref() == Some(NULL_DEVICE)),
            "GIT_CONFIG_GLOBAL override must remain: {envs:?}",
        );
        assert!(
            envs.iter()
                .any(|(k, v)| k == "GIT_CONFIG_SYSTEM" && v.as_deref() == Some(NULL_DEVICE)),
            "GIT_CONFIG_SYSTEM override must remain: {envs:?}",
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
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
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
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();

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

    #[test]
    fn remove_checkout_keeps_the_branch() {
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let checkout = work.path().join("f1").join("a");

        let prepared = prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout)
            .expect("prepare");
        assert!(commit_in(&checkout, "out.txt"), "commit");

        remove_checkout_keep_branch(&prepared.repo_root, work.path(), &checkout);

        assert!(!checkout.exists(), "checkout must be removed");
        assert!(
            git_ref_exists(&prepared.repo_root, "refs/heads/fleet/f1/a").unwrap(),
            "the fleet branch (the deliverable) must survive checkout removal",
        );
    }

    // Whole body is unix-only (needs `symlink`), like its sibling below —
    // gating the test itself keeps the Windows build warning-free.
    #[cfg(unix)]
    #[test]
    fn prepare_rejects_symlinked_checkout() {
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
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
        let repo = tempfile::tempdir().unwrap();
        if !git_init_repo(repo.path()) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // work/f1 -> <outside> (a symlinked parent of the checkout).
        std::os::unix::fs::symlink(outside.path(), work.path().join("f1")).unwrap();
        let checkout = work.path().join("f1").join("a");
        assert!(
            prepare_fleet_worktree(repo.path(), work.path(), "fleet/f1/a", &checkout).is_err(),
            "a symlinked PARENT component must be refused (no create/remove through it)",
        );
    }

    #[test]
    fn assert_checkout_contained_rejects_escape() {
        // HIGH #2: a checkout not under fleet-work is rejected outright.
        let work = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(
            assert_checkout_contained(work.path(), &elsewhere.path().join("a")).is_err(),
            "a checkout outside fleet-work must be refused",
        );
        // A normal contained checkout is accepted.
        assert!(assert_checkout_contained(work.path(), &work.path().join("f1").join("a")).is_ok(),);
    }

    #[test]
    fn prepare_rejects_non_repo() {
        let plain = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let checkout = work.path().join("f1").join("a");
        assert!(
            prepare_fleet_worktree(plain.path(), work.path(), "fleet/f1/a", &checkout).is_err(),
            "a non-git controller workspace must be refused",
        );
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
