# Tool capabilities behind the standard dispatcher

Run `octos chat`. The runtime exposes four common tools and two fixed gateways.
All capabilities below remain registered when authorized; optional ones are
reached through `discover_tools` and `run_tool`. See [dispatcher usage](fixed-tool-dispatcher.md).

## Audit and decisions

| Original capability | Useful? | Implementation decision |
|---|---|---|
| `read_file`, `view_image` | Yes: source and visual evidence | Present `read_file`; retain image attachment and legacy `view_image` execution. Add `files` batches. |
| `write_file`, `edit_file` | Yes: new files and precise edits | Keep both. `edit_file.edits` already applies several replacements in one call. |
| `apply_patch`, `diff_edit` | Yes: multi-file edits and unified diffs | Discover `apply_patch`, which already accepts both patch text and `{path, diff}`. Keep the legacy entry point. |
| `exec_command`, `bash` | Yes: running and verifying work | Present `exec_command`, with PTY, persistent sessions and recoverable output. Keep legacy `bash` execution. |
| `shell`, `write_stdin` | Yes: detached jobs and interactive sessions | Keep: detached supervisor jobs and PTY input/polling are distinct lifecycle operations. |
| `glob`, `grep`, `list_dir` | Yes: efficient code discovery | Keep typed searches, contextual matches and directory inspection. Replacing them with Bash loses useful output and validation contracts. |
| `web_search`, `web_fetch`, `browser` | Yes: external evidence and UI work | Batch search/fetch requests; retain browser separately because it has interactive state and actions. |
| `spawn`, `spawn_agent`, `delegate`, send/resume/wait/close | Yes: parallel work | Preserve lifecycle semantics. `wait_agent(include_output=true)` collects status, artifact paths and final output together. |
| `check_background_tasks`, `read_task_output`, peer tools | Yes: supervised/external work | Preserve session-scoped inspection, log paging and peer identity. These are dynamically registered, not always part of the base count. |
| workspace log/show/diff, `check`, contract check | Yes: review and verification | Keep exact validation and history operations. They are not equivalent to reporting progress in prose. |
| `tool_search`, `tool_suggest` | Yes: integration discovery | Discover `tool_search`; both use the live registry catalog. |
| `ask_user_question`, `request_user_input` | Yes: missing user input | Prefer the answer-routed `ask_user_question`; retain the older entry point for compatibility. |
| `update_plan` | Yes: user-visible progress | Keep; do not confuse planning with execution evidence. |
| `view_video`, optional `git`/`code_structure` | Yes: distinct media/code capabilities | Keep when built and authorized. |
| unbound `image_generation` | Stub is not usable | Omit the stub from discovery. A bound implementation is discoverable when authorized. |

An exact host allowlist still controls access. If it excludes the gateways,
its authorized native tools remain directly advertised. Historical tool calls
remain executable through their original permission, hook and sandbox paths.

## Batching that reduces calls

Read several files or images in one model-visible call:

```json
{"files":[{"path":"src/main.rs","start_line":20,"limit":60},{"path":"screenshots/result.png"}]}
```

For `web_search` and `web_fetch`, the batch field is `requests`. Each item uses
the original single-request parameters, for example:

```json
{"requests":[{"query":"official documentation topic A"},{"query":"official documentation topic B"}]}
```

Batches report individual failures, preserve request order and pass all items
through the original read/network checks. A call accepts up to 32 requests and
runs up to four I/O reads concurrently; these are per-call resource bounds,
not limits on agents or worker count. Cancellation drops pending reads. Output
fits the existing tool budget and identifies truncated items. Truncated batch
reads do not mark files as completely observed in the file-state cache. Image
bytes are delivered to the main model and saved immutably by the existing
executor. Path-filtered hooks inspect every file in a read batch.

Collect completed agents without a separate result call:

```json
{"targets":["exact-task-id"],"timeout_ms":30000,"include_output":true,"max_output_bytes":4096}
```

The output budget is shared across targets. Unknown or out-of-session IDs are
not treated as completed. Missing final output is represented as `null`, not
as a successful empty answer. Large logs still use `read_task_output` paging.

## Boundaries and validation

The standard fixed interface is measured and tested in
[dispatcher documentation](fixed-tool-dispatcher.md). Discovery and execution
preserve native hooks, authorization, cancellation, images and progress tracking.
Dependent commands and verification still require separate execution steps.
Batching tests cover scope, SSRF, truncation, file-state caching and images.
