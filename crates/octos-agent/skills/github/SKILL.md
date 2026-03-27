---
name: github
description: GitHub CLI (gh) for issues, PRs, repos, releases, and actions. Triggers: github, issue, pull request, PR, repo, release, actions, workflow, gist, gh.
version: 1.0.0
author: octos
always: false
---

# GitHub CLI (gh)

You can interact with GitHub using the `gh` CLI tool via `shell`. Use it to manage issues, pull requests, repositories, releases, and more.

## Prerequisites

The `gh` CLI must be installed and authenticated (`gh auth login`).

## Issues

```bash
gh issue list                              # list open issues
gh issue list --state all --limit 20       # all issues
gh issue view 123                          # view issue details
gh issue create --title "Bug" --body "..."  # create issue
gh issue close 123                         # close issue
gh issue comment 123 --body "Fixed in #456" # add comment
gh issue list --label "bug" --assignee "@me" # filter
```

## Pull Requests

```bash
gh pr list                                 # list open PRs
gh pr view 456                             # view PR details
gh pr create --title "Fix" --body "..."    # create PR
gh pr merge 456                            # merge PR
gh pr checkout 456                         # check out PR locally
gh pr diff 456                             # view PR diff
gh pr review 456 --approve                 # approve PR
gh pr checks 456                           # view CI status
gh pr comment 456 --body "LGTM"            # comment on PR
```

## Repository

```bash
gh repo view                               # current repo info
gh repo view owner/repo                    # specific repo
gh repo clone owner/repo                   # clone repo
gh repo create name --public               # create repo
gh repo list owner                         # list repos
```

## Releases

```bash
gh release list                            # list releases
gh release view v1.0.0                     # view release
gh release create v1.0.0 --notes "..."     # create release
gh release download v1.0.0                 # download assets
```

## Actions / Workflows

```bash
gh run list                                # list workflow runs
gh run view 12345                          # view run details
gh run watch 12345                         # watch run in progress
gh workflow list                           # list workflows
gh workflow run deploy.yml                 # trigger workflow
```

## Search

```bash
gh search repos "topic" --limit 10         # search repos
gh search issues "bug" --repo owner/repo   # search issues
gh search prs "fix" --state open           # search PRs
```

## API (Advanced)

```bash
gh api repos/owner/repo                    # raw API call
gh api repos/owner/repo/pulls/123/comments # PR comments
gh api graphql -f query='{ viewer { login } }' # GraphQL
```

## Best Practices

1. Use `gh pr create` with `--body` to include a description — don't create empty PRs.
2. Check `gh pr checks` before merging to ensure CI passes.
3. Use `gh issue list --label` to filter by labels for targeted work.
4. Prefer `gh pr view` over opening a browser for quick PR inspection.
5. Use `gh api` for operations not covered by built-in commands.
