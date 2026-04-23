# Octos Harness — Third-Party Skill Compatibility Contract

This document defines the compatibility contract a third-party skill must
uphold in order to be safely installed, executed, reloaded, and removed
through the Octos harness. It is the productization boundary for M4.4 of the
harness master plan and must stay truthful about what the runtime actually
enforces today.

If a runtime change breaks any of the guarantees listed here, the change is
a breaking change to the customer surface and needs an explicit migration
path.

## Scope

The contract covers:

- installable skill packages (Git, Octos Hub registry, or local path)
- custom-app skills that produce deliverable artifacts
- one non-slides reference fixture — the summary app at
  `e2e/fixtures/compat-test-skill/`

Out of scope:

- first-party slides/site runtime branches
- any new marketplace mechanism beyond the current registry shape
- prompt-only skills without binaries (covered by the skill developer guide)

## Minimum skill shape

A compatible skill is a directory containing at minimum:

```
my-skill/
├── manifest.json   # tool contract
├── SKILL.md        # documentation + stable frontmatter
└── main            # executable (or pre-declared binary per platform)
```

### Stable developer-interface fields

Skills MUST only depend on the documented stable fields from
`docs/app-skill-dev-guide.md`. Runtime-internal types, private modules, or
undocumented path conventions are NOT part of the contract and may change
at any time.

**`manifest.json`** (documented fields only):

- `name`, `version`, `author`, `description`
- `timeout_secs`, `requires_network`, `sha256`
- `tools[].name`, `tools[].description`, `tools[].input_schema`
- `tools[].spawn_only`, `tools[].spawn_only_message`
- `tools[].env` — env-var *names* the tool is allowed to receive
- `mcp_servers[]`, `hooks[]`, `prompts`
- `binaries` — platform-keyed pre-built binary download info

**`SKILL.md` frontmatter** (documented fields only):

- `name`, `description`, `version`, `author`
- `always`, `requires_bins`, `requires_env`

### Binary protocol

The `main` executable MUST obey the stdin/stdout JSON protocol:

- `argv[1]` = tool name
- stdin = JSON object matching `tools[].input_schema`
- stdout = JSON object with at minimum `{"output": <string>, "success": <bool>}`
- optional: `"files_to_send": [<absolute-path>, ...]` to declare deliverable
  artifacts
- exit code: `0` for success, non-zero for failure
- stderr is diagnostic only

A skill MUST NOT write secret values to stdout, stderr, or produced
artifacts. See "Secret handling" below.

## Lifecycle guarantees

When installed through any supported source (Git URL, GitHub shorthand, or
local path) the Octos runtime guarantees that the following lifecycle
operations work without per-skill runtime branches:

1. **Install** copies the skill tree into the target profile's
   `<profile-data>/skills/<skill-name>/`. Pre-built binaries from
   `manifest.json.binaries` are preferred; the registry entry is the
   secondary source; `cargo build --release` / `npm install` is the fallback.
2. **Discovery** — `octos skills list` (and the programmatic `list_skills`
   API) surfaces the skill name, version from SKILL.md frontmatter, tool
   count from manifest, and installed-from source.
3. **Run** — the runtime spawns `main <tool_name>` with the documented
   environment policy. Tool calls that request background execution via
   `spawn_only: true` are supervised as durable task objects per the harness
   developer interface.
4. **Deliver** — artifacts returned through `files_to_send` are the
   authoritative deliverables for the call. Declared artifacts are persisted
   alongside the child task state and survive session reload.
5. **Reload** — a skill installed and running before a reload is still
   visible through `octos skills list` and its `.source` tracking file
   after the reload. The artifacts it has already produced are still
   reachable.
6. **Remove** — `octos skills remove <name>` deletes the entire skill
   directory, including any `main`, pre-built binary, `node_modules`, or
   `target/` built during install. Removal is **idempotent**: removing an
   already-absent skill succeeds silently (matches HTTP DELETE semantics).

### Install/remove failure expectations

Every failure mode must be actionable without reading runtime logs:

- missing source / invalid source → CLI prints `git clone failed` or
  `Local path not found` with the requested path
- missing SKILL.md in source → `No SKILL.md found in <path>`
- platform without pre-built binary + no Cargo/npm → explicit message
  naming `cargo not found` or a missing `package.json`
- remove on unknown name → idempotent success (no error)
- path-traversal attempt on remove → rejected with `Invalid skill name`

The same errors propagate through the REST admin API with appropriate HTTP
status codes (400/404), and through the in-chat `/skills remove` surface.

## Sandbox expectations

Skill binaries inherit the Octos sandbox policy, which is enforced uniformly
regardless of skill origin:

- the runtime uses the active sandbox backend (`Bwrap`, `Macos`,
  `Docker`, or `NoSandbox`)
- environment variables in the shared `BLOCKED_ENV_VARS` set are stripped
  before spawn (LD_PRELOAD, DYLD_*, NODE_OPTIONS, PYTHONPATH, and so on)
- file paths flagged with injection characters are rejected by sandbox
  path validation
- symlinks in the installed skill tree are rejected during install
- binary size is capped at 100 MB
- SHA-256 integrity is verified when `sha256` is declared in `manifest.json`
- tool timeouts are clamped to `[1, 600]` seconds

A skill MUST NOT assume it can escape the sandbox or disable any of the
blocked environment variables. The runtime does not expose a knob to do so.

## Secret handling

Secrets are passed to a skill by **env-var name**, never by literal value
embedded in the skill package.

Rules:

1. Declare the required env var in `SKILL.md` frontmatter via
   `requires_env: FOO_TOKEN,BAR_KEY`.
2. Additionally allowlist the env var in `manifest.json` under
   `tools[].env`, otherwise the runtime strips secret-like variables from
   the spawned plugin process.
3. The skill MUST NOT print the secret value to stdout or stderr and MUST
   NOT write it to any produced artifact (including error messages,
   debug logs, summaries, or delivered files).
4. The compat gate test enforces (3) by running the fixture with a known
   canary value and asserting that the value never appears in any captured
   surface.

The harness itself applies additional filtering: API keys, passwords, and
other secret-shaped env vars are denied by default and only forwarded when
the manifest's `tools[].env` explicitly allowlists them.

## Executable layout

After a successful install the skill's directory layout is guaranteed:

```
<profile-data>/skills/<name>/
├── main              # executable (unless the skill provides only extras)
├── manifest.json     # tool definitions
├── SKILL.md          # documentation
├── .source           # install tracking (repo/branch/installed_at)
└── <other files>     # styles/, prompts/, hooks/, bundled assets
```

The `main` binary is marked executable (Unix mode `0o755`). Generated
lazy-cargo wrappers that shell out to `cargo build --release` at first run
are treated as install-time stubs, not as a satisfied binary, to make sure
the runtime never serves a stub as if it were the real skill.

## Declared-artifact format

A skill declares artifacts it wants delivered through the `files_to_send`
field in its JSON response:

```json
{
  "output": "Summary written to /tmp/compat/summary.md",
  "success": true,
  "files_to_send": ["/tmp/compat/summary.md"]
}
```

Rules:

- paths MUST be absolute
- each path MUST exist by the time the skill exits
- files are delivered in order; the first path is treated as the primary
  artifact when no explicit `primary_artifact` field is exposed
- the size of any single file is bounded by the sandbox I/O limits; large
  artifacts should be streamed through the file-sending tool chain rather
  than inlined in stdout

Artifact delivery runs through the same policy-owned path as first-party
slides and sites, so a custom app does not need to ask for special
runtime code.

## Compatibility gate

The checked-in compatibility gate covers:

| Layer           | File                                                     |
| --------------- | -------------------------------------------------------- |
| Fixture skill   | `e2e/fixtures/compat-test-skill/`                        |
| Rust int. test  | `crates/octos-cli/tests/skill_compat_gate.rs`            |
| Browser e2e     | `e2e/tests/skill-compat-gate.spec.ts`                    |
| Contract        | this document                                            |

The Rust integration test is hermetic — it installs the fixture from a
local path into a temporary directory, invokes the binary protocol
directly, asserts artifact delivery, verifies the skill survives a "reload"
(fresh `octos skills list`), removes the skill, and then proves uninstall
idempotency.

The browser spec drives the same flow against a live canary through the
admin dashboard. The supervisor owns canary dispatch; this spec only
authors the flow.

### Adding a new compat fixture

When a new custom-app class needs coverage (report, audio, research,
coding assistant, etc.), add a sibling directory under `e2e/fixtures/` and
extend the Rust integration test with one new `#[test]` function. The test
name should follow `should_<expected>_when_<condition>` and exercise the
full install-run-deliver-reload-remove cycle.

Do NOT add per-app branches to the runtime. If the new fixture needs a
runtime capability that does not exist yet, that is a harness development
task (M4.x) rather than a compat-gate extension.

## Release gate

A release is blocked until:

- `cargo test -p octos-cli --test skill_compat_gate` is green on CI
- the browser spec is dispatched against at least one live canary
- this document, the fixture, and the two tests all continue to reference
  only documented developer-interface fields

Regressions in any of the four lifecycle assertions (install / run /
reload / remove) MUST be treated as release blockers. Custom apps that
depend on the stripped field set have no other place to go.
