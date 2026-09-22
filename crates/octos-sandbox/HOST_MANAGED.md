# Host-managed agent confinement

The host must launch workers with `host_managed_command(absolute_executable)`.
Attach private piped stdin/stdout/stderr, then pass `acp --host-managed`. The
launcher clears the environment and working directory and prevents inheritance
of unrelated file descriptors. Do not restore credentials or user configuration
through environment variables. A child's protocol `confined` flag is not an
attestation; the parent-enforced launcher is the security boundary.

The CLI calls `confine_host_managed()` on its initial thread before constructing
Tokio, reading configuration, creating agent memory, or accepting host input.
Failure is terminal. There is no unconfined fallback. Episodic memory is in RAM;
no workspace, scratch directory, disk database, plugins, or local tools are needed.

## macOS

The parent uses the system `/usr/bin/sandbox-exec` and a default-deny Seatbelt
profile. Private file contents, metadata and directory enumeration, network
(including loopback and Unix sockets), subprocess creation, other task ports,
keychain/Mach service lookup, and persistent writes are denied. The worker
validates the inherited restrictions and closes startup descriptors.

Seatbelt can only be installed once. Runtime read and executable-map grants
therefore remain for the exact worker binary, `/usr/lib`, `/System/Library`, the
OS Cryptex runtime, and Apple's `dyld-support.sb` bootstrap paths/ancestors.
No home, workspace, preferences, or general `/etc` grant exists. The selected
binary can reexec itself, inheriting identical confinement; it cannot fork or
execute another program. The host retains the same compartment identity across
any child behavior. Direct standalone startup installs a stricter profile with
no executable/runtime-file grants. Existing broader inherited sandboxes are not
supported substitutes for the parent launcher.

The runtime also permits self process information/signals, a small explicit CPU,
page-size and OS-release sysctl list, and null/entropy devices. Anonymous memory,
threads, kqueue, clocks and inherited pipe I/O remain usable. Missing system
sandbox support is an error. This does not promise App Store/App Sandbox nesting.

## Linux

Requires system `bubblewrap` (`/usr/bin/bwrap` or `/bin/bwrap`) version 0.8 or
later, enabled unprivileged user/mount/PID/IPC/network namespaces, seccomp, and
fully enforced Landlock ABI 3 (Linux 6.2 or later). Disabled features fail closed.
Windows, Android and other platforms are unsupported and return an error.

Before the worker enters, bubblewrap creates a separate read-only filesystem,
PID/IPC/network/UTS namespaces, drops capabilities, and installs a syscall
allowlist supplied through a sealed, consumed descriptor. Only the selected
binary at `/app/octos`, its actual ELF interpreter and dependency files, and
null/entropy devices are mounted. `lddtree` parses the dependency graph without
executing the binary. Dependencies must resolve to root-owned, non-writable
files below standard system library roots with protected ancestor directories.
There are no mounts of whole library trees, `/etc`, loader caches, `/proc`, homes,
workspaces, or temporary directories. File metadata outside this synthetic
filesystem is inaccessible; Landlock alone would not provide that property.

The child adds Landlock and a stricter syscall filter before processing input.
Only shared-address-space pthread clones are allowed; process clones, exec,
sockets to other processes, io_uring, ptrace/process-memory APIs, device ioctl, keyrings, namespaces,
and unknown syscalls are denied. `clone3` returns ENOSYS so libc can use the
checked clone path. No filter is removed or weakened. Startup is single-threaded
by contract; all subsequently created threads inherit both restriction layers.
Anonymous Unix stream pairs remain available for Tokio's internal signal
self-pipe; socket creation/connect/bind/accept, datagram pairs and FD passing are
denied, so those pairs cannot communicate with another process.

## Validation

`cargo test -p octos-sandbox --test host_managed` runs real subprocess probes for
the parent boundary, child initialization, and direct standalone confinement.
They attempt private content/metadata/directory access, writes, inherited-FD
reads, local TCP/UDP/Unix networking, subprocesses, parent memory and macOS
keychain access, while checking async Tokio stdin/stdout and threads still work.

The native probes also run in ordinary package and workspace test suites. Linux
test hosts must meet the requirements above; missing confinement is a test
failure, not a skipped check. Hosted Ubuntu CI jobs use
`scripts/setup-host-managed-linux-ci.sh` to provision their disposable runners.
Self-hosted runners require administrator-provisioned prerequisites; their
workflows do not change the machine's AppArmor or namespace policy.

Set `OCTOS_HOST_MANAGED_BINARY` to an absolute built `octos` path to also run an
actual confined ACP handshake and create a memory-only session using a synthetic
host tool catalog. The dedicated CI workflow runs both checks on macOS and Linux.
OS isolation does not eliminate timing/resource side channels or kernel exploits.
