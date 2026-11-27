# Octos GitHub Action

Run the Octos agent on a repository's issues and pull requests, from a
workflow in that repository.

- **Mention.** Write `@octos` in an issue, a pull request comment or a review
  comment. Octos reads the thread and the checkout and answers in a comment.
  With `access: write` it can also make the change.
- **Review.** Comment `@octos review` on a pull request, or turn on
  `auto_review`. Octos reads the change and posts one review with a comment
  on each line it has a finding for. It never approves and never blocks.
- **Prompt.** Give the action a fixed `prompt` and it runs on every event that
  reaches the workflow: triage of new issues, a nightly report, a check on
  each pull request.
- **Pull request watch.** Comment `@octos watch` on a pull request, or give it
  the `octos-watch` label. From then on Octos looks at every failed CI run and
  every review from a maintainer. It pushes a fix when the fix is clear and
  small, and asks in a comment when it is not.

The agent itself gets no GitHub token and no network. It reads the checkout,
may edit files, and returns a message. The action does every GitHub call in
fixed code: it reads the thread, posts the comment, commits and pushes.

## Quick start

1. Add your model key as a repository secret, for example `DEEPSEEK_API_KEY`.
2. Copy [`examples/octos.yml`](examples/octos.yml) to
   `.github/workflows/octos.yml` on the default branch.
3. Comment `@octos what does this function do?` on an issue.

The core of that workflow:

```yaml
permissions:
  actions: read
  contents: write
  issues: write
  pull-requests: write

jobs:
  octos:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
      - uses: octos-org/octos/action@main # pin a release tag or a commit SHA
        env:
          DEEPSEEK_API_KEY: ${{ secrets.DEEPSEEK_API_KEY }}
        with:
          provider: deepseek
          model: deepseek-flash
          access: write
```

The model key goes in `env`, under the name the provider reads. `provider`,
`model`, `base_url` and `api_type` are passed to `octos chat` as they are, so
any provider Octos supports works, including a self-hosted endpoint.

Start with `access: read`. Octos then answers and explains but changes
nothing. [`examples/issue-triage.yml`](examples/issue-triage.yml) goes one
step further and keeps the answer in the run summary, so nobody is notified
while you tune the prompt.

## What starts a run

| Event | Condition | What Octos does |
|---|---|---|
| `issue_comment`, `pull_request_review_comment` | A new comment mentions `@octos` | Answers the comment |
| `issues`, `pull_request` | Opened or edited, and the text newly mentions `@octos` | Answers the text |
| `issues`, `pull_request` with `labeled` | The label is `label_trigger` | Works on the issue or pull request |
| `issue_comment` on a pull request | The comment starts with `@octos review` | Reviews the pull request |
| `pull_request` with `opened`, `reopened`, `ready_for_review`, `synchronize` | `auto_review` is on | Reviews the pull request |
| `issue_comment` on a pull request | The comment is `@octos watch` or `@octos unwatch` | Sets or removes the watch label. No model run |
| `pull_request_review` | The pull request is watched | Goes through the review's points |
| `workflow_run` | The run failed and belongs to a watched pull request | Reads the failed jobs' logs and looks for the cause |
| Any | `prompt` is set | Runs the prompt |

Who may start a run:

- Only users with write access to the repository. Anyone else is ignored
  without a comment.
- `allowed_non_write_users` and `allowed_bots` open the action to other
  senders. Their runs are always read-only, whatever `access` says.
- `workflow_dispatch`, `schedule`, `push`, `repository_dispatch` and
  `merge_group` count as started by a maintainer.
- A comment the action wrote itself never starts a run.

## Pull request watch

A watched pull request is one that carries the watch label and whose branch
is in the same repository. Pull requests from forks are never watched.

- **CI failure.** The workflow listens to `workflow_run` for the workflows you
  name. On a failure the action finds the pull request, checks that the failed
  commit is still its head, and gives Octos the names of the failed jobs and
  steps and the end of each log.
- **Review.** A review from a user with write access starts a run with the
  review text and its line comments. An approval without line comments does
  not.
- **Fixes.** With `access: write` Octos edits the checkout and the action
  commits to the pull request's branch. Each such commit carries an
  `Octos-Autofix: 2 of 3` trailer. After `max_fix_attempts` commits the action
  says so once and stops. A mention still works after that.
- **Answers.** One comment per run. For a CI failure it is tied to the commit,
  so a second failed workflow on the same commit does not start a second run.

Commits pushed with the default `GITHUB_TOKEN` do not start workflows, so CI
does not run on a fix pushed that way. Pass the token of a GitHub App, or a
fine-grained personal access token with write access to contents, issues and
pull requests, as `github_token` to have CI run on each fix.

## Pull request review

Octos posts one review per run: a summary, and a comment on each line it has
a finding for. Each finding is marked Important, Nit or Pre-existing.

- **On request.** A user with write access comments `@octos review` on the
  pull request. The command must start the comment. Text after it says what
  to look at, as in `@octos review the error handling`. This works on drafts
  and on pull requests from forks too. For a fork Octos works from the diff,
  because a fork's code is never checked out.
- **By itself.** With `auto_review: "true"` the action reviews on the
  `pull_request` events `opened`, `reopened`, `ready_for_review` and
  `synchronize`. The `types` list of your workflow decides which of them
  reach it: leave out `synchronize` to review once, when the pull request
  opens. Drafts, pull requests from forks and a commit that already has a
  review are skipped. [`examples/pr-review.yml`](examples/pr-review.yml) is a
  complete workflow.

What to expect from a review:

- It is read-only, whatever `access` says. Its event is always `COMMENT`, so
  it never approves and never requests changes.
- A line comment goes only on a line the diff shows. The action checks every
  finding against the diff after the agent has finished. When the line number
  is off, it uses the line whose text the agent quoted. What cannot be placed
  is listed in the review text.
- On a later review of the same pull request Octos is shown the findings it
  already posted and is told not to repeat them.
- The repository's own rules go in `REVIEW.md` at the root of the base
  branch: what counts as Important, what to skip, what to always check. The
  pull request's own copy of that file is not used, so the author of a change
  does not set the rules for its review.

## Inputs

| Input | Default | Meaning |
|---|---|---|
| `prompt` | | Instructions for a run without a mention |
| `trigger_phrase` | `@octos` | Text that calls Octos |
| `label_trigger` | | Label that asks Octos to work on an issue or pull request |
| `watch_label` | `octos-watch` | Label that marks a watched pull request |
| `access` | `read` | `read` or `write`. `write` lets Octos edit files, and the action pushes them |
| `comment` | `true` | Post the answer as a comment. `false` keeps it in the run summary and the `result` output |
| `allowed_non_write_users` | | Logins without write access that may start a read-only run, or `*` |
| `allowed_bots` | | Bot logins that may start a read-only run, or `*` |
| `allowed_tools` | | Octos tools or groups to allow on top of files, shell and search, for example `group:web`. `*` lifts the list |
| `max_fix_attempts` | `3` | Automatic fix commits per watched pull request |
| `auto_review` | `false` | Review pull requests on the `pull_request` events that reach the action |
| `branch_prefix` | `octos/` | Prefix of the branch pushed for an issue |
| `dry_run` | `false` | Read from GitHub and run the agent, but post and push nothing |
| `provider`, `model`, `base_url`, `api_type`, `effort` | | Passed to `octos chat` |
| `max_iterations` | `50` | Upper bound on agent steps |
| `timeout_minutes` | `20` | The agent is stopped after this time |
| `require_sandbox` | `true` | Refuse to run shell commands when the runner has no sandbox |
| `setup_sandbox` | `true` | On Linux, install bubblewrap and let it create user namespaces. Needs passwordless sudo |
| `config_file` | | Octos config file in the repository to start from |
| `octos_version` | `latest` | Octos release to install |
| `octos_sha256` | | Expected SHA-256 of the release bundle |
| `octos_path` | | An `octos` binary already on the runner. Skips the download |
| `github_token` | `github.token` | Token for reading, commenting and pushing |
| `commit_name`, `commit_email` | `github-actions[bot]` | Author of the commits |

## Outputs

| Output | Meaning |
|---|---|
| `should_run` | `true` when the event asked for a run and the sender was allowed |
| `skip_reason` | Why nothing ran |
| `kind` | `mention`, `label`, `prompt`, `code_review` (Octos reviews), `review` (Octos answers a maintainer's review) or `ci_failure` |
| `conclusion` | `success` or `failure` of the agent run |
| `result` | The agent's final message |
| `pushed`, `branch_name`, `commit_sha` | The commit the action made, if any |
| `review_url`, `review_comments` | The review the action posted, and how many line comments it holds |

## How a run works

1. **Read the event.** The action decides whether the event asks for a run,
   checks the sender's permission, and fetches the thread: title, description,
   comments, and for a pull request the changed files and the diff. On a pull
   request from the same repository it checks out the pull request's branch.
   It removes the git credentials that `actions/checkout` stored, and posts a
   "working on this" comment.
2. **Install Octos.** The release bundle is downloaded and checked against its
   SHA-256.
3. **Run the agent.** `octos chat` runs once, without a session, with
   `--sandbox read-only` or `--sandbox workspace-write`, and with approval
   prompts off, so a command that would need approval is refused.
4. **Publish.** The comment is replaced by the answer. With `access: write`
   the changed files are committed and pushed: to the pull request's branch,
   or for an issue to a new branch, with a link that opens a pull request for
   it. The action does not open the pull request itself. For a review there
   is no comment and no commit: the action posts the review instead.

## Security

Octos acts on text that anyone can write into an issue. The action is built
so that such text cannot reach beyond the checkout.

**What the agent cannot do**

- It has no GitHub token. The token is not in its environment, and the stored
  checkout credentials are removed before it starts.
- Its shell runs in the Octos sandbox with no network. It cannot write outside
  the checkout, and with `access: read` it cannot write at all. With
  `require_sandbox` a runner without a sandbox refuses commands.
- Its tools are limited to files, shell and search in the checkout. Web
  search, page fetching, the browser, sub-agents and memory are off unless
  `allowed_tools` or your own `config_file` turns them on.
- It cannot change what the action does after it. The permission decision
  travels between steps inside the runner, and the steps that follow the agent
  run from a copy of the script kept outside the checkout.

**Text from other people**

- Only the request of a user with write access is given to the agent as a
  request. Everything else, such as the issue text, other comments, diffs and
  logs, is placed in marked data blocks, and the agent is told not to follow
  instructions found there.
- Hidden HTML comments, invisible characters, image alt text, link titles,
  hidden HTML attributes and character entities are removed first.

**What gets published**

- In the answer, `@name` mentions other than the person who asked are made
  inert, so the answer does not notify people or start another run. Values of
  secret environment variables are replaced by `[redacted]`.
- Changes under `.github/workflows/` are never committed. Neither are nested
  repositories.
- Before committing, the action puts `.git/config` back to the copy it took
  before the agent ran, moves HEAD back to the starting commit, and runs git
  with hooks and helper programs off. A hook or a redirected remote the agent
  left in `.git` has no effect.
- A changed file that holds the value of a secret environment variable stops
  the commit.

**What is left to you**

- Keep `access: read` until you trust the setup. `write` is for repositories
  with branch protection on the default branch.
- Do not check out a fork's code before this action in a workflow that has
  secrets. The action itself never does.
- The model key is in the Octos process. Octos keeps it out of the commands it
  runs, and the action redacts it from the answer, but treat it as reachable
  by a determined prompt injection: use a key with a spending limit.
- The agent reads the whole checkout, and its answer is a public comment on a
  public repository.
- On Linux with bubblewrap the shell sees the system folders and the checkout
  only. On macOS it can also read some files outside the checkout, the
  runner's temp folder among them.
- `allowed_tools: group:web` gives a prompt injection a way to send out what
  the agent can read.
- Later steps of the same job find the checkout without stored git
  credentials.

## Limits

- One comment or one review per run. Octos does not reply inside review
  threads and does not resolve them.
- A review comment sits on one line. Octos offers no one-click suggestions:
  a `suggestion` block in a finding is shown as plain code.
- A review sees up to 300 changed files and about 60 KB of diff. Beyond that
  Octos is told which files were left out.
- For a CI failure Octos sees the failed run only: up to six failed jobs and
  the end of each log.
- It does not resolve merge conflicts.
- On a pull request from a fork a mention gets a read-only answer based on the
  diff. The fork's code is not checked out.
- The prompt is capped at 100 KB. Long threads and large diffs are cut.
- Runners: Linux on x86-64 and arm64, macOS on arm64. No Windows.

## Development

```bash
python3 action/octos_action.py --self-test
```

The self-test covers the trigger rules, the permission rules, the prompt, and
with real git repositories the commit path, including what an agent could
leave behind in `.git`. It needs no network.

The review path is covered there too, against a stand-in for the GitHub API:
the split of the agent's message into findings, the line checks, and what
happens when GitHub refuses a line or the pull request moves on.

[`.github/workflows/action-e2e.yml`](../.github/workflows/action-e2e.yml) runs
the whole action on a hosted runner against
[`dev/mock_llm.py`](dev/mock_llm.py), a scripted stand-in for the model, so it
needs no secret. The script has the agent try to read the tokens, reach the
network, write outside the checkout, plant a git hook and add a workflow file,
and [`dev/e2e_check.py`](dev/e2e_check.py) checks that none of it worked.
