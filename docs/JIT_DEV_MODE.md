# Running Octos from source with live patching

**Status: experimental developer mode.** Checked on macOS ARM64 only. Linux
and Windows are untested. Nothing here changes how Octos is built or released:
release binaries come from cargo exactly as before, and no crate depends on
Relax.

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
scripts/jit/setup-relax.sh
```

The script fetches one pinned Relax commit, applies the fixes in
[`scripts/jit/relax-patches`](../scripts/jit/relax-patches/README.md) and
builds the `relax` binary under `~/.octos-jit/relax`. Set `OCTOS_RELAX_DIR` to
put it elsewhere. Relax reads its own `core`, `alloc` and `std` from the
checkout it was built in, so that folder has to stay where it was built.

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
call, with the tool name as the argument and the JSON input on stdin.
`scripts/jit/skill-jit` runs that program from its Rust source, so an edit to
a skill is in effect on the next call. There is no build and no install step.

```bash
scripts/jit/skill-jit run weather get_weather '{"city":"Tokyo"}'
scripts/jit/skill-jit check weather
scripts/jit/skill-jit link weather /absolute/path/to/data/skills
scripts/jit/skill-jit unlink weather /absolute/path/to/data/skills
```

The skill is a folder name under `crates/app-skills` or
`crates/platform-skills`, or the path of any skill crate that has a
`Cargo.toml` and a `manifest.json`. A crate outside this checkout works too.

- `run` makes one tool call. The call runs in the folder it is started in.
- `check` type- and borrow-checks the skill and runs nothing.
- `link` puts a development copy of the skill into a skills folder: its
  `manifest.json`, its `SKILL.md` and a `main` that is a short script. The
  script runs the source through Relax on every call.
- `unlink` removes that copy. Both commands leave a skill folder alone that
  `link` did not make.

Link into `<data-dir>/skills`. Octos reads that folder before its bundled
skills, and the first skill with a given id wins, so the linked copy takes the
place of the bundled one. A server reads its skill folders when it starts:
link first, or restart it.

What to expect:

- **A server started by `octos-jit` has no bundled skills.** Octos copies them
  from beside its own executable, and under the JIT that executable is
  `relax`. Link the skills the session needs.
- **A broken edit fails the call.** A skill process is new for every call, so
  there is no previous code to keep running. The tool returns the compile
  error with file and line, and works again once the source compiles.
- **The linked manifest has a longer timeout.** A call after an edit compiles
  first, so `link` raises a `timeout_secs` below 180 to 180
  (`SKILL_JIT_TIMEOUT`). It also leaves out a `sha256` or `signature` of the
  built executable, because `main` is not that file; a server that sets
  `plugins.require_signed` does not load a linked skill. Nothing else changes.
  Link again after a change to `manifest.json` or `SKILL.md`.
- **`link` ends with one warm-up call.** It passes a tool name no skill has
  and no input, so the skill has nothing to act on, and the cache then holds
  what a real call starts from. `--no-warm` skips it.
- **The environment is written into `main`.** Octos gives a skill a reduced
  environment. `link` records what the JIT needs: the folder of `cargo`, the
  workspace, and `CARGO_HOME`, `RUSTUP_HOME` and `RELAX_HOME` when they are
  set. Pass `--env NAME` or `--env NAME=VALUE` for anything else the build
  needs.
- **Build scripts run as a release build.** The pptx skill refuses a release
  build without its fonts:

  ```bash
  source scripts/prepare-pptx-fonts.sh
  scripts/jit/skill-jit link --env CRAFT_FONTS_DIR --env CRAFT_FONTS_REQUIRED \
    pptx-runtime /absolute/path/to/data/skills
  ```

- **`deep-search` needs `cmake` on `PATH`**, as it does with cargo: it builds
  BoringSSL.

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
| Tool call of a linked skill in an agent turn, time skill | 0.4 to 0.5 s; 0.7 to 3.2 s for the first call after an edit |
| Direct call of the time skill through its linked `main` | About 0.2 s; the binary cargo builds takes about 0.06 s |
| First tool call of a server that runs under the JIT itself | 20 s, the server compiles its own skill path first |
| First run of a skill on an empty cache | 17 to 43 s |
| Rebuild of the time skill with cargo after an edit | 0.7 to 3.4 s, then an install |
| pptx skill through its linked `main` | One slide built and rendered in 4.1 to 6.1 s; a small action in about 2.5 s |

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
- **The skill cache depends on the `relax run` options.** Changing
  `SKILL_JIT_FLAGS` compiles the skill again on the next call.
- **C code in a skill's dependencies can stay unoptimised.** Relax builds it
  at opt-level 0 first and swaps in the optimised build from the background
  of a later run. A skill call can end before that finishes: after the runs
  measured here, three of eight native libraries in the cache were still the
  opt-level 0 builds.
