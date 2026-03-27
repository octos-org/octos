---
name: git
description: Git version control operations via shell. Triggers: git, commit, branch, merge, diff, log, clone, push, pull, rebase, stash.
version: 1.0.0
author: octos
always: false
---

# Git Version Control

You can perform git operations using the `shell` tool. Use git for version control tasks like committing, branching, diffing, and managing repositories.

## Common Operations

### Status and History
```bash
git status
git log --oneline -20
git diff
git diff --staged
git show HEAD
```

### Branching
```bash
git branch                    # list branches
git branch feature-name       # create branch
git checkout feature-name     # switch branch
git checkout -b feature-name  # create and switch
git merge feature-name        # merge branch
```

### Committing
```bash
git add <files>               # stage specific files
git commit -m "message"       # commit with message
git commit --amend            # amend last commit
```

### Remote Operations
```bash
git pull origin main
git push origin branch-name
git fetch origin
git remote -v
```

### Stash
```bash
git stash                     # stash changes
git stash list                # list stashes
git stash pop                 # apply and drop
```

### Investigation
```bash
git log --oneline --graph --all -20    # visual branch history
git blame <file>                        # line-by-line authorship
git log --follow -p -- <file>          # file history with diffs
git reflog                              # recent HEAD movements
```

## Best Practices

1. Always check `git status` before committing to see what will be included.
2. Write clear, concise commit messages describing the "why" not the "what".
3. Use `git diff` to review changes before staging.
4. Prefer creating new commits over amending published commits.
5. Never force-push to shared branches without explicit permission.
6. Stage specific files rather than `git add .` to avoid committing unintended files.
