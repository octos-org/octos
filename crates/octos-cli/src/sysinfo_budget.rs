//! task-sysinfo-proc-stat-fd-budget: keep `sysinfo` from holding `/proc`
//! handles open for the lifetime of the server.
//!
//! On Linux `sysinfo` caches an open `/proc/<pid>/stat` (and per-task
//! `/proc/<pid>/task/<tid>/stat`) file for every process it indexes — up to
//! HALF of `RLIMIT_NOFILE`, and it raises the soft limit to the hard limit to
//! get there. `octos serve` used to build its metrics `System` with
//! `System::new_all()`, which indexes every process and thread on the host at
//! startup and keeps those files open; nothing closes them until an admin
//! `system_metrics` poll happens to `refresh_all()` again. A 26-hour
//! `--stdio --solo` server was found holding ~1200 such handles, many for
//! long-dead pids (incident 2026-08-17).
//!
//! Policy here: no handle cache at all (`set_open_files_limit(0)` — every
//! refresh is open→read→close), no process snapshot at construction, and the
//! metrics endpoint refreshes only what it renders.

/// Build the metrics `System` for `AppState`: handle cache disabled, no
/// process snapshot. CPU usage becomes meaningful from the second refresh,
/// exactly as before (a fresh `new_all()` also needed two samples).
#[cfg(feature = "api")]
pub(crate) fn new_metrics_system() -> sysinfo::System {
    // Idempotent process-wide setting; must precede the first process
    // refresh. Returns false on non-Linux, where nothing is cached anyway.
    let _ = sysinfo::set_open_files_limit(0);
    sysinfo::System::new()
}

/// Refresh exactly what `system_metrics` renders. Processes are refreshed
/// only on request, without per-thread tasks, dropping dead entries.
#[cfg(feature = "api")]
pub(crate) fn refresh_metrics(sys: &mut sysinfo::System, include_procs: bool) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    if include_procs {
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
    }
}
