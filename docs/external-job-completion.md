# External job completion

External tools can return a job id while a renderer, image service, or build
continues outside Octos. A fixed shell sleep cannot learn when that job finishes.
`watch_external_job` connects the service's local JSON receipt to Octos's existing
background-task supervisor and terminal notifications. The runtime observes the
file every 500 ms; this does not spend an LLM turn per poll.

## Producer contract

Publish a regular UTF-8 JSON file (at most 256 KiB) in the session workspace or
another directory the session is allowed to read. Write a temporary file and
atomically rename it over the receipt whenever the status changes:

```json
{
  "id": "render-123",
  "request_sha256": "the-immutable-request-hash",
  "status": "running",
  "result": null,
  "error": null
}
```

Use `queued`, `pending`, or `running` while active. Finish with `completed`,
`failed`, `cancelled` (also accepts `canceled`), or `interrupted`. `completed`
means the external operation succeeded; it does not mean its artifacts passed
visual QA or the user's acceptance criteria. The service may include result
paths and other metadata in the receipt. Octos does not execute any receipt
content or automatically deliver its files.

## Agent use

### Automatic registration from a tool adapter

Adapters should emit the following in `ToolResult.structured_metadata`:

```json
{"octos_external_job":{"schema_version":1,"receipt_path":"requests/tool-jobs/render-123.json","job_id":"render-123","request_sha256":"the-immutable-request-hash","status":"running"}}
```

Shell/bash/exec_command adapters may instead print this as a complete JSON
object, or a compact JSON event line within the last 32 output lines. The
kernel recognizes this explicit envelope; it never guesses jobs from prose
or arbitrary `read_file` content. Receipt paths must be workspace-relative,
without traversal. Only queued/pending/running jobs register a watch.

Registration happens through the existing tool dispatcher, preserving hook,
provider/host policy, typed permissions, session ownership and supervisor
limits. The original tool result includes the native task handle; no invented
assistant tool call is added to the conversation. The same receipt/id/request
tuple is registered at most once per registry. Failed registration releases
the claim; a watch that later times out can still be explicitly re-watched.
Registry rebinding/restart clears the in-memory claims.

The integration suite includes an actual shell command that emits the receipt
twice, then independently replaces it on completion. It requires one native
task and one terminal notification, with no model-authored watch call.

### Explicit registration

After submitting the job once, call:

```json
{
  "receipt_path": "requests/tool-jobs/render-123.json",
  "job_id": "render-123",
  "request_sha256": "the-immutable-request-hash",
  "timeout_secs": 1800
}
```

`receipt_path` and `job_id` are required; the hash pin is optional. Register one
watch per job. The built-in registry marks this tool for background execution:
the agent gets a native task handle immediately and can continue independent
work. The supervisor emits its existing session-scoped terminal event when the
receipt finishes; OUP and gateway hosts use their existing continuation wiring.
Do not add fixed `sleep` commands to wait for this event. On completion, read
the receipt's result paths and perform the appropriate artifact checks.

An already completed receipt returns immediately. A missing file or incomplete
JSON is retried within the deadline. Wrong ids, mismatched hashes, unsupported
statuses, denied paths, and unreadable or oversized files produce explicit
observation errors. Timeout is reported as `watch_status: "timed_out"` with
`job_terminal: false` and the last observed job status. It does not assert that
the external job failed and never resubmits it.

The watcher opts out of the background executor's generic transient-error
retry: even an external failure saying `connection refused` must be reported
immediately, rather than delaying notification or resetting the watch deadline.

Cancelling the Octos watcher stops observation; it does **not** cancel an
external operation. After an Octos restart, re-watch the same durable receipt
to recover its state, including jobs that finished while Octos was offline.
Watchers are not automatically re-created across restart. A host embedding the
kernel must wire the ordinary supervisor/notification callbacks to schedule
continuations; the tool itself does not start an LLM turn. Direct tool execution
without the agent loop awaits the final result instead of returning a handle.

This version supports local receipts, including receipts synchronized by a
remote-job adapter. It does not discover arbitrary HTTP jobs or interrupt
unrelated foreground commands. Existing adapters must emit the receipt
envelope, or the agent must explicitly register a watch. It does not rewrite
arbitrary shell programs or restart observers from an unreturned receipt.

## Verification

`cargo test -p octos-agent --test external_job_completion --no-default-features`
exercises a real agent loop with a scripted provider: one watch call releases
the foreground, an independent producer replaces the receipt, and the native
supervisor and result sender notify the parent without another polling call.
The suite also covers failure, cancellation, identity, bounded waiting, partial
receipts, workspace rebinding, policy denial, and symlink escapes.

### Operational metadata and output redaction

The execution layer captures the validated receipt envelope before text is
truncated or sanitized for the model. This is necessary because ordinary
output sanitization redacts 64-character hex strings, including SHA-256.
Watcher identity must use the original hash; model-facing text keeps the
existing redaction behavior. The real-shell regression test uses a full
SHA-256 and verifies both successful notification and redacted model output.
