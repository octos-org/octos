# Running Octos from source with live patching

**Status: experimental developer mode.** Checked on macOS ARM64 only. Linux
and Windows are untested. Nothing here changes how Octos is built or released:
release binaries come from cargo exactly as before, and no crate depends on
Relax. The skill commands described below are part of the `octos` binary and
do nothing until Relax is set up.

`scripts/jit/octos-jit` runs this checkout through
[Relax](https://github.com/makepad/relax), a JIT compiler for Rust, instead of
a cargo build. Relax compiles the workspace from source inside the process and
watches the files. A saved edit is compiled and patched into the running
server without a restart. An edit that does not compile is refused and the
previous code keeps running.

Use it to shorten the edit-and-try loop on the server, and to experiment with
an agent that changes its own harness while it runs. Do not use it to serve
real users.

## Setup

```bash
octos skills jit setup        # with an installed octos
scripts/jit/setup-relax.sh    # the same from a checkout
```

Both fetch one pinned Relax commit, apply the fixes in
[`scripts/jit/relax-patches`](../scripts/jit/relax-patches/README.md) and
build the `relax` binary under `~/.octos-jit/relax`. The `octos` binary
carries the script and the fixes, so the first form needs no checkout. Set
`OCTOS_RELAX_DIR` to put the build elsewhere. Relax reads its own `core`,
`alloc` and `std` from the checkout it was built in, so that folder has to
stay where it was built. `octos skills jit status` shows which Relax and which
cargo would be used.

It needs `git`, a Rust toolchain with `cargo`, and a C compiler. The fixes are
not upstream. Relax itself is days old and changes many times a day, so the
pin is deliberate; see the patch README before moving it.

## Run

```bash
scripts/jit/octos-jit serve --solo --data-dir /absolute/path/to/data
```

Everything after the script name goes to `octos`. Relax starts a package from
its workspace folder, so the process runs with the checkout as its working
directory: give paths as absolute paths and pass `--cwd` where a command takes
one.

The first run compiles the C dependencies with cargo and fills `target/relax`.
It takes minutes. Later runs start from that cache. The first line on stderr
names the process:

```text
relax-remote: pid 75089 app octos-cli socket /tmp/makepad-501/makepad-remote-75089.sock (relax inspect 75089 help)
```

The launcher builds `octos-cli` with `--no-default-features --features api`.
The default feature set builds llama.cpp and BoringSSL through cmake and has
not been made to work under Relax. `OCTOS_JIT_FEATURES` replaces the feature
arguments.

To run the terminal client the same way, point
`scripts/jit/octoscode-jit` at an octoscode checkout (`OCTOSCODE_SRC`, default
a folder named `octoscode` next to this one) and let it start the server:

```bash
cd /path/to/your/project
/path/to/octos/scripts/jit/octoscode-jit \
  --stdio-command "/path/to/octos/scripts/jit/octos-jit serve --shared --solo --data-dir /absolute/path/to/data"
```

The client waits 60 s for its server. On a cold cache that is not enough: it
exits with `Cannot attach to the Octos server`, while the server it started
keeps starting. Run the client again once the server log shows `Listening`.
The first connection also compiles the WebSocket path, so a second retry can
be needed. The client's stderr goes to `OCTOSCODE_JIT_LOG`, by default
`octoscode-jit.log` in the temporary directory.

## Editing while it runs

Save a file. Relax reports each save on the server's stderr:

```text
relax live: list_dir.rs: patched (1 item(s) changed, 1 fn(s) installed) in 1774.9 ms
relax live: list_dir.rs: NOT applied (errors below); the app runs the previous code
```

`relax inspect <pid> live` prints the same verdict for scripts: `patches`,
`restart_pending` and `pending_why`.

| Edit | What happens |
| --- | --- |
| A fn body, a new fn, a const or static, a new struct with a trait impl, a new import, a changed signature | Patched live |
| A type or borrow error | Refused; the previous code keeps running |
| A trait gains a method while trait objects of it exist; the layout of a live value changes | `restart_pending: 1`. `relax inspect <pid> session relaunch` restarts in place, same pid, in-memory state lost |
| A relaunch while the sources do not compile | The process does not come back |
| An edit that compiles and then panics on the main thread | The process dies |

Patching replaces code, not data. Anything the old code already built stays
as it was built. A session's tool registry is made when the session starts, so
a tool added by a live edit is visible to new sessions only.

`relax inspect <pid> pin on` holds every edit back until `pin off`, which then
lands them as one patch. Use it to change several files without the process
seeing a half-done edit.

## Developing a skill

A skill is a separate program. Octos starts its `main` once for every tool
call, with the tool name as the argument and the JSON input on stdin. Octos
can run that program from its Rust source through Relax, so an edit to a skill
is in effect on the next call. There is no build and no install step.

```bash
octos skills new my-skill                                    # a starter crate in ./my-skill
octos skills run ./my-skill my_skill '{"text":"hello"}'      # one call from source
octos skills check ./my-skill                                # type and borrow check, runs nothing
octos skills --profile alice dev ./my-skill                  # let alice's agent use the source
octos skills --profile alice remove my-skill                 # take it out again
```

The skill is the folder of a skill crate: `Cargo.toml`, `manifest.json` and
`SKILL.md`. `run` and `check` also take the name of a skill that `dev`
registered. Without `--profile` the commands work on `.octos/skills` in the
current folder, as the other `octos skills` commands do.

- `new` writes a starter crate: `Cargo.toml`, `src/main.rs`, `manifest.json`
  and `SKILL.md`, with one tool that echoes a text. It needs no Relax and
  builds with cargo as well.
- `run` makes one tool call, in the current folder. The JSON input is the
  argument, else stdin, else `{}` on a terminal.
- `check` type- and borrow-checks the skill and runs nothing.
- `dev` registers the crate as a **dev skill**, under the name of its folder:
  the name `octos skills install ./my-skill` would give it. The skill's folder
  in the skills directory then holds a marker, `.octos-dev.json`, a copy of
  `manifest.json` and `SKILL.md`, and a `main` that is a short script. The
  script runs the source through Relax on every call. The source folder itself
  is not written to. `dev` also records the registration in `.octos-dev/` in
  the skills directory, outside the skill's folder: a `.octos-dev.json` that
  arrives inside an installed skill is ignored.
- `remove` takes the registration out. `list` and `info` show a dev skill
  with its source folder. `dev` does not replace an installed skill of the
  same name unless it is given `--force`.

A profile's skills folder is read before the bundled skills, and the first
skill with a given id wins, so a dev skill takes the place of the bundled one.

What to expect:

- **A server reads its skills when it starts.** Restart one that is already
  running after the first `dev`. From then on a source edit needs nothing. A
  change to `manifest.json` or `SKILL.md` needs a restart only: Octos copies
  both from the source again each time it loads the skill.
- **A broken edit fails the call.** A skill process is new for every call, so
  there is no previous code to keep running. The tool returns the compile
  error with file and line, and works again once the source compiles.
- **The timeout is at least 180 seconds.** A call after an edit compiles
  first, so a skill that runs from source gets that much whatever
  `timeout_secs` says.
- **A dev skill is not signed.** The copy of the manifest leaves out a
  `sha256` or `signature`, which pin the built executable, and a server that
  sets `plugins.require_signed` refuses a dev skill.
- **`dev` ends with one warm-up call.** It passes a tool name no skill has and
  no input, so the skill has nothing to act on, and the cache then holds what
  a real call starts from. `--no-warm` skips it. When the skill does not
  compile, `dev` prints the error and exits with a failure; the registration
  stays.
- **The environment is written into `main`.** Octos gives a skill a reduced
  environment. `dev` records what the JIT needs in the marker: the Relax
  binary, the folder of `cargo`, the workspace, and `CARGO_HOME`,
  `RUSTUP_HOME` and `RELAX_HOME` when they are set. Pass `--env NAME` or
  `--env NAME=VALUE` for anything else the build or the skill needs. Run `dev`
  again after Relax or the toolchain moved.
- **Only the manifest and `SKILL.md` are copied.** Other files beside the
  skill stay in the source folder.
- **Build scripts run as a release build.** The pptx skill refuses a release
  build without its fonts:

  ```bash
  source scripts/prepare-pptx-fonts.sh
  octos skills --profile alice dev --env CRAFT_FONTS_DIR --env CRAFT_FONTS_REQUIRED \
    crates/app-skills/pptx-runtime
  ```

- **`deep-search` needs `cmake` on `PATH`**, as it does with cargo: it builds
  BoringSSL.
- **A server started by `octos-jit` has no bundled skills.** Octos copies them
  from beside its own executable, and under the JIT that executable is
  `relax`. Register the skills the session needs with `dev`.

### Installed skills that ship as source

A skill installed from a repository that has no built executable for this
machine gets a generated `main` that builds it with `cargo build --release` on
its first call. With `OCTOS_SKILLS_SOURCE_RUNTIME=relax` in the server's
environment, and Relax set up, the generated `main` runs the source through
the JIT instead: no release build, and the same limits as a dev skill. Octos
writes that `main` when it loads the skill and replaces it when the setting
changes. A skill with a real executable, or with a `sha256` in its manifest,
is never touched.

The two differ after an edit to the installed source. Under Relax the next
call runs the edited source. The cargo `main` builds once and keeps running
that binary until `target/release` is removed.

### Letting the agent develop a skill

With `OCTOS_SKILLS_AGENT_DEV=1` in the server's environment, and Relax set
up, the agent gets one more tool, `skill_dev`. It is off without the setting.

| Action | What it does |
|---|---|
| `new` | Writes the starter crate of `octos skills new` into the session's workspace. |
| `check` | Type-checks a skill crate and returns the compiler's errors. |
| `run` | Calls one tool of the crate with a JSON input and returns what it printed. |
| `register` | Registers the crate as a dev skill of the profile, as `octos skills dev` does. |

The agent edits the source with its file tools between calls, so it can write
a tool, read the compile errors, fix them and call the tool in one turn. After
`register` the skill's tools are regular tools from the next server start.
Until then the agent reaches them through `run`.

`check`, `run` and `register` compile, run or keep code the model wrote, and
they do so outside the shell sandbox. Each one asks for approval first, and is
refused when the client has no approval prompt. An approval for the session
covers the later `skill_dev` calls of that session. The crate must lie in the
session's workspace. The child process does not get the server's API keys, and
it is stopped after 300 s.

## An agent that edits its own harness

Run with the checkout as the agent's workspace and the agent can change the
server that is executing it. Observed with a real model
(`deepseek-v4-flash`):

- The agent edited its own `list_dir` tool and got the new output from the
  same tool later in the same turn. Same process, no restart.
- It added a new tool. The three files patched live; a new session could call
  it, the running session could not.
- An edit with a type error was refused and the server stayed healthy.
  `git checkout` of the file rolled the behaviour back in 0.2 s.

This is the mechanism only. Nothing in Octos scores a change and decides
whether to keep it. Three things to know before building that:

- **The agent cannot work the gate itself.** Octos runs shell commands under a
  sandbox that denies network access, and that blocks the Relax control
  socket: `relax inspect` from the agent's shell answers `no running app`. A
  supervisor outside the sandbox can pin and unpin. With the server pinned
  from outside, the agent's edit was on disk and its tool still returned the
  old text; after the unpin the patch landed in 1.8 s.
- **A change that compiles can still kill the process.** Keep the supervisor
  outside the process it supervises, and keep approval, sandbox and
  authentication code out of what the agent may edit unreviewed.
- **cargo stays the authority.** Relax has its own type checker, borrow
  checker and standard library. Code can pass under Relax and fail under
  rustc. Every change still has to pass the normal cargo checks before it
  merges, and code that calls Relax's own API needs a cargo feature that is
  off by default.

## Measured

On one Apple Silicon Mac, 2026-11-13.

| What | Result |
| --- | --- |
| `relax selftest`, every fn of the package compiled | `octos-cli` 697,283 fns and octoscode 128,085 fns, 0 failures |
| Cold start of the server on an empty cache | 135 s |
| Warm start of the terminal client and its server to a ready session | 6 to 7 s |
| Live patch of one tool file in the server | 4 to 8 s for the first patch of a file, 0.2 to 1.8 s after |
| The same edit with cargo | 74 s incremental `cargo build` on a busy machine, then a restart |
| Memory | 2 to 7 GB per process |
| Cache in `target/relax` | About 14 GB for Octos, 7 GB for octoscode |
| Server starts with a client connecting at once, after patch 0006 | 108 starts, no task ref-count panic |
| `relax selftest` over the 17 skill packages | 683,621 fns, 0 failures |
| Tool call of a dev skill in an agent turn, time skill | 0.4 to 0.5 s; 0.7 to 3.2 s for the first call after an edit |
| Direct call of the time skill through its generated `main` | About 0.2 s; the binary cargo builds takes about 0.06 s |
| First tool call of a server that runs under the JIT itself | 20 s, the server compiles its own skill path first |
| First run of a skill on an empty cache | 17 to 43 s |
| First run of a new starter skill on an empty cache | 4 to 8 s |
| `octos skills jit setup` on an empty folder | 131 s |
| Rebuild of the time skill with cargo after an edit | 0.7 to 3.4 s, then an install |
| pptx skill through its generated `main` | One slide built and rendered in 4.1 to 6.1 s; a small action in about 2.5 s |

## Known problems

- **Stalls on first use.** The first run of a code path waits for one JIT
  thread to compile it, and every program thread waits with it. Once in 40
  starts the server took 26 s to exit after SIGTERM instead of about 0.5 s.
  Wait for the old server to be gone before starting another on the same data
  directory; one started during that stall failed with `Database already
  open` on `admin_audit.redb`.
- **Diagnostic noise.** After the first live patch the server logs several
  hundred lines ending in `Relax bug, report it` about missed inlining. The
  server kept working.
- **`std::env::current_exe()` is the `relax` binary.** Code that re-executes
  itself does not work; one `octos-core` test fails for this reason.
- **Starting the server on a pseudo-terminal failed once** with `cargo failed
  building the native dependencies`. Starting it from the terminal client or
  without a terminal works. The cause is not known.
- **The terminal client gives up on a cold cache**, as described under Run.
- **No default features**, as described under Run.
- **A skill call under the JIT is slower than a built skill.** Each call
  starts Relax and loads the cache: a few tenths of a second for a small
  skill. For a small skill cargo rebuilds in seconds, so the gain is the
  missing build and install step, not speed.
- **The skill cache depends on the `relax run` options.** Octos always uses
  `--allow-c --silent --unattended`. A hand-made `relax run` of the same skill
  with other options compiles it again.
- **C code in a skill's dependencies can stay unoptimised.** Relax builds it
  at opt-level 0 first and swaps in the optimised build from the background
  of a later run. A skill call can end before that finishes: after the runs
  measured here, three of eight native libraries in the cache were still the
  opt-level 0 builds.
