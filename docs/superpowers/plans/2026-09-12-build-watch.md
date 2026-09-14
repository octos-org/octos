# Git metadata watch implementation plan

Linked worktrees store `.git` as a pointer file. Watching nonexistent
`.git/HEAD` makes Cargo rerun the version build script on every build.

1. Resolve existing HEAD, current loose branch ref and packed-refs paths with Git.
   Watch the refs directory when a packed branch has no loose ref yet, so the
   next same-branch commit is detected. Always watch build.rs itself.
2. Accept Git ownership only when the expected project root has `.git` and
   canonical `git rev-parse --show-toplevel` matches it. Let Git resolve pointer
   files, including relative paths. Archives nested in other repositories retain
   an empty Git hash even when the parent tracks their files.
3. Exercise the exact production build script in tiny offline Cargo fixtures:
   normal/linked no-op builds, same-branch commits, branch and detached changes,
   packed/pruned refs, relative gitdir, Git-less and tracked/untracked archives.
   Assert actual compiler invocations and the executable's embedded hash.
4. Delete only fixture-owned directories. Never invoke Git cleanup from a
   directory that could resolve an unrelated ancestor repository.
5. Run fmt, clippy and all-targets tests for octos-cli with its api feature;
   independently review the source and recorded command exits before commit.

kache is a separate optional cache experiment; this fix does not install it or
change global Cargo configuration.

## Claude review follow-up (2026-09-12)

Approved scope: make the regression detectors independent of inherited Cargo
colors and avoid rebuilding for unrelated remote refs after packing a branch.

- [ ] Reproduce the existing test under `CARGO_TERM_COLOR=always`, and add a
  real Cargo positive/negative control with an intentionally missing watch path.
- [ ] Add ordinary and linked packed-branch fixtures that fetch local remote
  updates, stay Fresh, then commit on the current branch and refresh the hash.
  Include disabled reflogs and a missing nested branch directory.
- [ ] Force `cargo build --color never` in the fixture subprocess and check the
  first compile in Fresh tests. Watch the nearest existing parent of the current
  branch ref; keep the wider fallback only when narrower parents do not exist.
  This retains direct ref correctness without depending on reflog settings.
- [ ] Update the behavior contract; run fmt, clippy, all-targets and color
  matrix checks. Verify the old build script still fails the no-op regression.
- [ ] Review the final diff, commit owned files and update PR #2310 and its CI.
