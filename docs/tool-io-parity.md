# Tool image and command-output reliability

This change closes three native tool gaps independently of a model, prompt,
skill, or artifact workflow. It builds on the four-tool image-read work in
[tool-capability-parity.md](tool-capability-parity.md).

## MCP content and historical images

The MCP adapter preserves text, embedded text resources, resource links, and
structured results. Image content and image blob resources become native
`ToolResult.model_media` attachments. They follow the normal tool-history and
provider serialization path; no auxiliary vision model is called. Non-image
binary resources are saved to workspace-readable files with their URI, MIME
type, and path reported. Resource links are reported without fetching them.
Invalid, unsupported, or oversized attachments produce an explicit diagnostic.
The existing MCP error status is preserved. Binary attachments share a 20 MiB
budget per result, with at most 16 images.

Before tool images enter history, Octos stores a private read-only snapshot
under the OS local application-data directory, `octos/model-media/`. Each file
is named by its SHA-256 digest inside a unique private directory. Later request
encoding checks that digest; it refuses changed bytes instead of silently
changing a historical image. The source file may be overwritten or deleted.
Snapshots survive process restarts and JSON transcript persistence. No schema
change is required: transcript media references now point to the snapshots.

This applies to newly captured tool images. Existing transcripts that still
refer to mutable source paths cannot recover past bytes retroactively. User
uploads and videos are outside this change. Snapshot retention is independent
of compaction: hosts should retain referenced files when exporting or moving
transcripts and may clean them up after deleting all referencing transcripts.
This change does not add automatic snapshot garbage collection.

Provider vision support and existing history-retention policy still apply.
For the tested OpenAI-compatible DeepSeek route, `retain_tool_images: true`
is still required to retain earlier image attachments across later requests.
It remains opt-in; this patch does not change model configuration defaults.

## Complete logs and bounded previews

Foreground `bash`, `exec_command`, and `shell` capture stdout and stderr to a
private file under `.octos/tool-output/` in their workspace. The combined stream
preserves observed arrival order; ordering between separate stdout/stderr pipes
is not guaranteed. Files are readable with the same scoped `read_file` tool.
The directory is self-ignored by Git so logging does not count as source edits.
Creating logs uses the confined no-symlink file-opening path. The workspace
must be writable for capture to start.

The in-memory tail is bounded at 64 KiB. Visible output is bounded separately;
when it is truncated, the response gives the full log path. Metadata contains
`full_output_path`, `output_start`, `output_end`, `truncated`, `log_complete`,
and `log_error`. Offsets are bytes in the combined stream, not token positions.
`log_complete` describes capture integrity so far, not process completion;
use `running`/`exit_code` for that. Capture errors or interrupted draining are
reported explicitly. Full logs are not automatically deleted with a poll.

Persistent command polls consume a cursor: `write_stdin` returns newly captured
output, never the entire accumulated buffer. If the new output exceeds the
preview budget, omitted bytes are recoverable from the full log. Polling an
already-read completed session returns no repeated command output.

`max_output_tokens` budgets the captured-output string using Octos's existing
context estimate (about four ASCII characters per token and 1.5 non-ASCII
characters per token). It is UTF-8 safe, not a byte limit or an exact provider
tokenizer. Session JSON envelope/metadata are outside this budget. Extremely
small budgets may truncate the explanatory footer; structured metadata always
contains the complete recovery path.

## Real terminals

`exec_command(tty=true)` allocates an OS pseudo-terminal using `portable-pty`.
Its stdin, stdout, and stderr are connected to the terminal; `write_stdin`
writes to its master. Commands with `yield_time_ms` and no TTY continue using
pipes. Both paths retain policy checks, sandbox wrapper commands, environment
filtering, timeouts, and process-group cleanup. Terminal output may contain
echo, CRLF, and ANSI escape sequences, as a real terminal normally does.

Terminal sessions are still process-local; this patch does not make a running
PTY reconnectable after a harness restart. Its saved log remains available.
The executable regressions run on macOS/Unix. Windows terminal behavior needs
platform CI validation; use of a portable library alone is not that validation.

## Regression evidence

- `tool_io_parity`: original-file overwrite/deletion, full-log recovery through
  scoped reads, nonrepeating polls, estimated token budgets, actual terminal
  file descriptors, terminal input, environment filtering, and write denial.
- `read_image_wire`: a real agent/tool/provider stack against a local HTTP
  endpoint verifies actual image bytes in successive main-model requests,
  including after the original file changes. No external model is required.
- MCP content tests cover images, embedded resources, links, structured data,
  saved binary bytes, invalid-image diagnostics, and preserved failure status.
- `mcp::wire_tests` runs the real rmcp handshake and tool transport through the
  adapter and agent to the main-model HTTP request, asserting the actual image
  payload, resource text, and tool-call pairing.
- Output-capture tests cover bounded memory, split UTF-8, display truncation
  metadata, log-path recovery, and symlink escape refusal.
- Vision tests cover actual-byte MIME detection and tampered-snapshot refusal.

These are transport and tool-contract checks. They do not claim improved model
reasoning, PPTX quality, or a deployed-server benchmark.

Validation on macOS:

```sh
cargo test -p octos-agent --no-default-features --lib \
  --test tool_io_parity --test read_image_wire --test security_sandbox \
  -- --test-threads=1
cargo test -p octos-llm --lib
cargo test -p octos-agent --no-default-features --lib -- --test-threads=4
cargo clippy -p octos-agent -p octos-llm --no-default-features --lib -- -D warnings
cargo fmt --all -- --check
git diff --check
```

The agent library passed 3,025 tests (3 ignored); the LLM library passed 768
(3 ignored). The image-wire, tool-I/O and isolation integrations passed 36
tests, also passing together under the default parallel runner. Clippy passed
with warnings denied. The original image, log, polling, TTY, and token-budget
regressions were reproduced before the implementation changes.

The final four-thread agent-library run also passed all 3,025 tests. Earlier
parallel runs intermittently failed subprocess-timeout cleanup assertions;
the focused cleanup tests and both final full runs passed. The cause of that
earlier intermittency was not established, and the existing process-signalling
implementation was not changed. A terminal-input test was changed to poll for
completion because real PTY input echo is not proof that the shell has finished.
