//! task-build-git-metadata-watch — the production build script must watch
//! REAL git metadata paths (`git rev-parse --git-path ...`), never the
//! nonexistent `../../.git/HEAD` / `../../.git/refs/heads/` shapes that a
//! linked worktree doesn't have (no-change rebuilds), and must refresh
//! `OCTOS_GIT_HASH` after a same-branch commit (HEAD text does not change
//! on a same-branch commit — the branch ref file does).
//!
//! The fixtures replicate the PRODUCTION build script byte-for-byte via
//! `include_str!("../build.rs")` into tiny no-dependency Cargo workspaces,
//! drive real `git` and real `cargo` (`env!("CARGO")`), and count actual
//! `rustc` invocations from `cargo build -vv` output — never string-contain
//! shortcuts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One tiny workspace: `dep` (no-dep lib) + `probe` (bin, build.rs copied
/// from the production file, prints OCTOS_GIT_HASH to prove env refresh).
fn write_fixture(root: &Path) {
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/dep\", \"crates/probe\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/dep/src")).unwrap();
    fs::write(
        root.join("crates/dep/Cargo.toml"),
        "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.join("crates/dep/src/lib.rs"),
        "pub fn one() -> u32 { 1 }\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/probe/src")).unwrap();
    fs::write(
        root.join("crates/probe/Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n[dependencies]\ndep = { path = \"../dep\" }\n",
    )
    .unwrap();
    // THE production script, byte-for-byte.
    fs::write(
        root.join("crates/probe/build.rs"),
        include_str!("../build.rs"),
    )
    .unwrap();
    fs::write(
        root.join("crates/probe/src/main.rs"),
        "fn main() { println!(\"hash={}\", option_env!(\"OCTOS_GIT_HASH\").unwrap_or(\"\")); }\n",
    )
    .unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `git add` ONLY the fixture SOURCE files — never a bare `add .` (the
/// fixture-target build outputs must not enter the repo history).
fn git_add_sources(dir: &Path) {
    for path in [
        "Cargo.toml",
        "crates/dep/Cargo.toml",
        "crates/dep/src/lib.rs",
        "crates/probe/Cargo.toml",
        "crates/probe/build.rs",
        "crates/probe/src/main.rs",
    ] {
        let out = Command::new("git")
            .args(["-C", dir.to_str().unwrap(), "add", "--", path])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git add runs");
        assert!(
            out.status.success(),
            "git add {path} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Commit the fixture sources at the CURRENT state (so both detached
/// commits carry the full Cargo tree) plus an optional extra tracked path.
fn commit_fixture(dir: &Path, msg: &str, extra: Option<&str>) {
    git_add_sources(dir);
    if let Some(e) = extra {
        let out = Command::new("git")
            .args(["-C", dir.to_str().unwrap(), "add", "--", e])
            .output()
            .expect("git add runs");
        assert!(out.status.success());
    }
    git(dir, &["commit", "-q", "-m", msg]);
}

fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    fs::write(dir.join("readme.txt"), "v1\n").unwrap();
    // Explicit adds only — never `add .` (fixture-target outputs live here).
    git(dir, &["add", "--", "readme.txt"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Run cargo in a fixture. Returns (exit, combined_log). Uses the SAME
/// fixture-local target dir across calls, `-vv` so rustc lines appear, and
/// a clean env (no inherited RUSTC_WRAPPER / CARGO_INCREMENTAL etc).
fn cargo_vv(ws: &Path, extra_env: &[(&str, &str)]) -> (i32, String) {
    let target = ws.join("fixture-target");
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    cmd.args(["build", "--offline", "-vv"])
        .current_dir(ws)
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_INCREMENTAL")
        .env_remove("RUSTFLAGS");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("cargo runs");
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), log)
}

/// Probe crate is Fresh ⇔ its `Running \`…rustc … --crate-name probe\` line
/// is ABSENT from the -vv log (a Fresh unit shows no rustc invocation).
fn probe_rustc_count(log: &str) -> usize {
    log.lines()
        .filter(|l| {
            l.contains("Running `") && l.contains("rustc") && l.contains("--crate-name probe")
        })
        .count()
}

fn probe_dirty(log: &str) -> bool {
    log.lines().any(|l| l.contains("Dirty probe"))
}

fn run_binary_hash(ws: &Path) -> String {
    let out = Command::new(ws.join("fixture-target/debug/probe"))
        .output()
        .expect("probe binary runs");
    assert!(out.status.success());
    let line = String::from_utf8_lossy(&out.stdout);
    line.trim()
        .strip_prefix("hash=")
        .unwrap_or_default()
        .to_string()
}

/// Self-cleaning tempdir: the guard's Drop removes the whole fixture
/// (repo, its worktree registrations under this tree, and the
/// fixture-target build outputs) so a test-fixing round never litters
/// rustc artifacts, even when an assertion fails mid-test.
struct FixtureDir(PathBuf);
impl FixtureDir {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "bwm-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        FixtureDir(base)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// -------------------------------------------------------------------------
// 1. Regular repo: second no-change build must be Fresh with 0 rustc calls.
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_regular_repo_second_build_fresh() {
    let dir_ws = FixtureDir::new("regular");
    let ws = dir_ws.path().to_path_buf();
    init_repo(&ws);
    write_fixture(&ws);
    let (e1, l1) = cargo_vv(&ws, &[]);
    assert_eq!(e1, 0, "first build: {l1}");
    assert_eq!(probe_rustc_count(&l1), 1, "first build compiles probe once");
    let (e2, l2) = cargo_vv(&ws, &[]);
    assert_eq!(e2, 0, "second build: {l2}");
    assert!(
        !probe_dirty(&l2),
        "second no-change build must be Fresh, got Dirty. Log tail:\n{}",
        l2.lines().rev().take(12).collect::<Vec<_>>().join("\n")
    );
    assert_eq!(
        probe_rustc_count(&l2),
        0,
        "no rustc invocation for probe on no-change rebuild"
    );
}

// -------------------------------------------------------------------------
// 2. Linked worktree: the ORIGINAL bug — ../../.git/HEAD does not exist
//    here, cargo marked the unit Dirty on EVERY build. Must now be Fresh.
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_linked_worktree_second_build_fresh() {
    let dir_main = FixtureDir::new("lw-main");
    let main = dir_main.path().to_path_buf();
    init_repo(&main);
    let dir_linked = FixtureDir::new("lw-linked");
    let linked = dir_linked.path().to_path_buf();
    let _ = fs::remove_dir_all(&linked);
    git(
        &main,
        &["worktree", "add", linked.to_str().unwrap(), "-b", "topic"],
    );
    write_fixture(&linked);
    commit_fixture(&linked, "fixture", None);
    let (e1, l1) = cargo_vv(&linked, &[]);
    assert_eq!(e1, 0, "linked first: {l1}");
    let (e2, l2) = cargo_vv(&linked, &[]);
    assert_eq!(e2, 0, "linked second: {l2}");
    assert!(
        !probe_dirty(&l2),
        "linked worktree no-change rebuild must be Fresh (was Dirty with ../../.git/HEAD missing). Log tail:\n{}",
        l2.lines().rev().take(12).collect::<Vec<_>>().join("\n")
    );
    assert_eq!(probe_rustc_count(&l2), 0);
}

// -------------------------------------------------------------------------
// 3. Same-branch commit: HEAD text is UNCHANGED, but the branch ref file
//    is rewritten — the watch must catch it and refresh OCTOS_GIT_HASH.
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_same_branch_commit_updates_hash() {
    let dir = FixtureDir::new("commit");
    let ws = dir.path().to_path_buf();
    init_repo(&ws);
    write_fixture(&ws);
    let (_e, _l) = cargo_vv(&ws, &[]);
    let before = run_binary_hash(&ws);
    assert!(!before.is_empty(), "hash is populated in a git repo");
    fs::write(ws.join("readme.txt"), "v2\n").unwrap();
    git(&ws, &["add", "--", "readme.txt"]);
    git(&ws, &["commit", "-q", "-m", "second"]);
    let (e3, l3) = cargo_vv(&ws, &[]);
    assert_eq!(e3, 0, "post-commit build: {l3}");
    let after = run_binary_hash(&ws);
    assert_ne!(
        before, after,
        "same-branch commit must refresh OCTOS_GIT_HASH (watch the branch ref, not HEAD text only)"
    );
    let short = git(&ws, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(after, short, "hash equals the NEW commit's short sha");
}

// -------------------------------------------------------------------------
// 4. Detached HEAD in a linked worktree: HEAD file itself carries the sha;
//    checking out a different commit must trigger a rebuild + hash update.
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_detached_head_change_triggers() {
    let dir_main = FixtureDir::new("det-main");
    let main = dir_main.path().to_path_buf();
    init_repo(&main);
    write_fixture(&main);
    commit_fixture(&main, "fixture-c1", None);
    // Second commit differs ONLY in the version-relevant readme (the full
    // Cargo tree exists at BOTH commits — switching must never land on a
    // commit missing Cargo.toml).
    fs::write(main.join("readme.txt"), "v2\n").unwrap();
    git(&main, &["add", "--", "readme.txt"]);
    git(&main, &["commit", "-q", "-m", "second"]);
    let first = git(&main, &["rev-parse", "HEAD~1"]);
    let dir_linked = FixtureDir::new("det-linked");
    let linked = dir_linked.path().to_path_buf();
    let _ = fs::remove_dir_all(&linked);
    git(
        &main,
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            &first,
        ],
    );
    let first_fixture = git(&linked, &["rev-parse", "--short", "HEAD"]);
    let _ = cargo_vv(&linked, &[]);
    assert_eq!(run_binary_hash(&linked), first_fixture);
    // Switch the detached HEAD to the other commit (same Cargo tree).
    let second = git(&main, &["rev-parse", "HEAD"]);
    git(&linked, &["checkout", "-q", &second]);
    let (e, l) = cargo_vv(&linked, &[]);
    assert_eq!(e, 0, "post-checkout build: {l}");
    let short2 = git(&linked, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(
        run_binary_hash(&linked),
        short2,
        "detached checkout refreshes the hash"
    );
}

// -------------------------------------------------------------------------
// 5. Packed refs: refs/heads/<b> may not exist as a loose file. Watch
//    packed-refs as well; a commit rewrites it (or creates the loose ref).
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_packed_refs_fresh_and_commit_triggers() {
    let dir = FixtureDir::new("packed");
    let ws = dir.path().to_path_buf();
    init_repo(&ws);
    write_fixture(&ws);
    git(&ws, &["pack-refs", "--all"]);
    let (e1, l1) = cargo_vv(&ws, &[]);
    assert_eq!(e1, 0, "packed first: {l1}");
    let (e2, l2) = cargo_vv(&ws, &[]);
    assert_eq!(e2, 0);
    assert!(
        !probe_dirty(&l2),
        "packed-refs repo, no-change rebuild Fresh. Log:\n{l2}"
    );
    assert_eq!(probe_rustc_count(&l2), 0);
    fs::write(ws.join("readme.txt"), "v2\n").unwrap();
    git(&ws, &["add", "--", "readme.txt"]);
    git(&ws, &["commit", "-q", "-m", "second"]);
    let (e3, l3) = cargo_vv(&ws, &[]);
    assert_eq!(e3, 0, "packed post-commit: {l3}");
    let short = git(&ws, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(
        run_binary_hash(&ws),
        short,
        "commit after pack-refs refreshes hash"
    );
}

// -------------------------------------------------------------------------
// 6. No git at all: no rerun-if-changed output may reference git paths,
//    build succeeds, hash empty. (The original script emitted
//    ../../.git/HEAD even here — nonexistent watch path.)
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_no_git_dir_builds_without_rerun() {
    let dir = FixtureDir::new("nogit");
    let ws = dir.path().to_path_buf();
    write_fixture(&ws);
    // We cannot observe the build script's stdout directly in the -vv log,
    // but a nonexistent watched path makes cargo mark the unit Dirty on the
    // SECOND build — so Fresh-on-rebuild IS the observable contract.
    let (e1, l1) = cargo_vv(&ws, &[]);
    assert_eq!(e1, 0, "no-git first build ok: {l1}");
    let (e2, l2) = cargo_vv(&ws, &[]);
    assert_eq!(e2, 0);
    assert!(
        !probe_dirty(&l2),
        "no-git rebuild must stay Fresh (original script watched nonexistent ../../.git/HEAD). Log:\n{l2}"
    );
    assert_eq!(run_binary_hash(&ws), "", "hash empty without git");
}

// -------------------------------------------------------------------------
// 7. Archive inside an OUTER git repo: git -C walks UP and would adopt the
//    outer repository's HEAD. The build script must NOT adopt it.
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_archive_inside_outer_repo_not_adopted() {
    let dir_outer = FixtureDir::new("outer");
    let outer = dir_outer.path().to_path_buf();
    init_repo(&outer);
    // A source archive extracted INSIDE the outer repo: no .git of its own,
    // and its files are NOT tracked by the outer repo.
    let archive_ws = outer.join("extracted");
    fs::create_dir_all(&archive_ws).unwrap();
    write_fixture(&archive_ws);
    let (e1, l1) = cargo_vv(&archive_ws, &[]);
    assert_eq!(e1, 0, "archive build ok: {l1}");
    assert_eq!(
        run_binary_hash(&archive_ws),
        "",
        "archive source must NOT adopt the outer repo's hash"
    );
    let (e2, l2) = cargo_vv(&archive_ws, &[]);
    assert_eq!(e2, 0);
    assert!(!probe_dirty(&l2), "archive rebuild stays Fresh: {l2}");
    // And adopting the outer repo would be observable: outer's hash differs.
    let outer_short = git(&outer, &["rev-parse", "--short", "HEAD"]);
    assert_ne!(outer_short, "", "outer repo has a hash");
}

// -------------------------------------------------------------------------
// 7b. Archive TRACKED by the outer repo (ROOT review): ls-files would say
//     tracked; the guard must still not adopt — the archive has no .git of
//     its own, so the owning-repo check (git-dir under the nearest
//     toplevel's .git entry) fails and we stay git-less.
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_tracked_archive_inside_outer_repo_not_adopted() {
    let dir_outer = FixtureDir::new("outer-tracked");
    let outer = dir_outer.path().to_path_buf();
    init_repo(&outer);
    let archive_ws = outer.join("extracted");
    fs::create_dir_all(&archive_ws).unwrap();
    write_fixture(&archive_ws);
    // The outer repo TRACKS the extracted files (the harder case: a naive
    // ls-files guard would adopt the outer repo).
    // Track the ARCHIVE SOURCE files explicitly (never `add .` — the
    // fixture-target outputs must not be tracked either).
    for path in [
        "extracted/Cargo.toml",
        "extracted/crates/dep/Cargo.toml",
        "extracted/crates/dep/src/lib.rs",
        "extracted/crates/probe/Cargo.toml",
        "extracted/crates/probe/build.rs",
        "extracted/crates/probe/src/main.rs",
    ] {
        git(&outer, &["add", "--", path]);
    }
    git(&outer, &["commit", "-q", "-m", "track archive"]);
    let (e1, l1) = cargo_vv(&archive_ws, &[]);
    assert_eq!(e1, 0, "tracked-archive build ok: {l1}");
    assert_eq!(
        run_binary_hash(&archive_ws),
        "",
        "tracked-by-outer archive must still NOT adopt the outer repo's hash"
    );
    let (e2, l2) = cargo_vv(&archive_ws, &[]);
    assert_eq!(e2, 0);
    assert!(
        !probe_dirty(&l2),
        "tracked-archive rebuild stays Fresh: {l2}"
    );
}

// -------------------------------------------------------------------------
// 5b. pack-refs --all --prune then a SAME-BRANCH commit: the commit
//     CREATES the loose ref file (HEAD text and packed-refs are both
//     unchanged) — watching only those two would MISS it. The refs-dir
//     watch catches the creation and refreshes the hash.
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_packed_pruned_same_branch_commit_refreshes() {
    let dir = FixtureDir::new("packed-prune");
    let ws = dir.path().to_path_buf();
    init_repo(&ws);
    write_fixture(&ws);
    git(&ws, &["pack-refs", "--all", "--prune"]);
    let short1 = git(&ws, &["rev-parse", "--short", "HEAD"]);
    let _ = cargo_vv(&ws, &[]);
    assert_eq!(run_binary_hash(&ws), short1);
    // Same-branch commit after pruned packing: creates refs/heads/main.
    fs::write(ws.join("readme.txt"), "v2\n").unwrap();
    git(&ws, &["add", "--", "readme.txt"]);
    git(&ws, &["commit", "-q", "-m", "second"]);
    let (e, l) = cargo_vv(&ws, &[]);
    assert_eq!(e, 0, "post-commit build: {l}");
    let short2 = git(&ws, &["rev-parse", "--short", "HEAD"]);
    assert_ne!(short1, short2);
    assert_eq!(
        run_binary_hash(&ws),
        short2,
        "loose-ref creation after pack-refs --prune must refresh the hash (refs-dir watch)"
    );
}

// -------------------------------------------------------------------------
// 8. Branch switch: HEAD text changes (ref: …main → ref: …topic); the
//    commit is different too. Rebuild must refresh the hash.
// -------------------------------------------------------------------------
#[test]
fn build_git_watch_branch_switch_triggers() {
    let dir = FixtureDir::new("switch");
    let ws = dir.path().to_path_buf();
    init_repo(&ws);
    write_fixture(&ws);
    commit_fixture(&ws, "fixture", None); // full tree at the common ancestor
    git(&ws, &["checkout", "-q", "-b", "topic"]);
    fs::write(ws.join("readme.txt"), "topic\n").unwrap();
    git(&ws, &["add", "--", "readme.txt"]);
    git(&ws, &["commit", "-q", "-m", "fixture-topic"]);
    let topic_short = git(&ws, &["rev-parse", "--short", "HEAD"]);
    let _ = cargo_vv(&ws, &[]);
    assert_eq!(run_binary_hash(&ws), topic_short);
    // Switch back to main — whose commit ALSO carries the full Cargo tree.
    git(&ws, &["checkout", "-q", "main"]);
    let (e, l) = cargo_vv(&ws, &[]);
    assert_eq!(e, 0, "post-switch build: {l}");
    let main_short = git(&ws, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(
        run_binary_hash(&ws),
        main_short,
        "branch switch refreshes the hash"
    );
}

#[test]
fn build_git_watch_relative_gitdir_and_linked_commit_refresh() {
    let holder = FixtureDir::new("relative");
    let main = holder.path().join("main");
    let linked = holder.path().join("linked");
    init_repo(&main);
    write_fixture(&main);
    commit_fixture(&main, "fixture", None);
    git(
        &main,
        &[
            "worktree",
            "add",
            linked.to_str().unwrap(),
            "-b",
            "relative",
        ],
    );
    fs::write(
        linked.join(".git"),
        "gitdir: ../main/.git/worktrees/linked\n",
    )
    .unwrap();
    let (exit, log) = cargo_vv(&linked, &[]);
    assert_eq!(exit, 0, "{log}");
    assert_eq!(
        run_binary_hash(&linked),
        git(&linked, &["rev-parse", "--short", "HEAD"])
    );
    let (exit, log) = cargo_vv(&linked, &[]);
    assert_eq!(exit, 0, "{log}");
    assert_eq!(probe_rustc_count(&log), 0, "relative gitdir no-op: {log}");
    fs::write(linked.join("readme.txt"), "linked commit\n").unwrap();
    commit_fixture(&linked, "linked change", Some("readme.txt"));
    let (exit, log) = cargo_vv(&linked, &[]);
    assert_eq!(exit, 0, "{log}");
    assert_eq!(
        run_binary_hash(&linked),
        git(&linked, &["rev-parse", "--short", "HEAD"])
    );
}

#[test]
fn build_git_fixture_cleanup_preserves_ancestor_worktrees() {
    let outer = FixtureDir::new("cleanup-parent");
    init_repo(outer.path());
    let linked = outer.path().join("stale-linked");
    git(
        outer.path(),
        &["worktree", "add", linked.to_str().unwrap(), "-b", "stale"],
    );
    fs::remove_dir_all(&linked).unwrap();
    let registration = outer.path().join(".git/worktrees/stale-linked");
    assert!(registration.exists());
    let archive = outer.path().join("archive");
    fs::create_dir(&archive).unwrap();
    drop(FixtureDir(archive));
    assert!(
        registration.exists(),
        "cleanup must not prune the ancestor repository"
    );
}
