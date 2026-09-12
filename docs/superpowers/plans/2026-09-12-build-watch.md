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
