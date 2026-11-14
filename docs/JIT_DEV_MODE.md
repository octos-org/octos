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
