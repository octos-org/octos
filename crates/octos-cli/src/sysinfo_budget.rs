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

#[cfg(test)]
mod tests {
    /// Structural guard, feature-independent: `System` is constructed in
    /// exactly one place (here) and never via the all-snapshotting
    /// `System::new_all()`.
    #[test]
    fn sysinfo_budget_module_owns_all_system_constructions() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read source");
                let rel = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let is_budget_module = rel == "sysinfo_budget.rs";
                for (i, line) in text.lines().enumerate() {
                    let l = line.trim_start();
                    if l.starts_with("//") {
                        continue;
                    }
                    // The budget module itself only mentions the pattern in
                    // docs/tests; every real construction site is elsewhere.
                    if !is_budget_module && l.contains("System::new_all()") {
                        offenders.push(format!("{rel}:{}: System::new_all()", i + 1));
                    }
                    if !is_budget_module && l.contains("sysinfo::System::new") {
                        offenders.push(format!(
                            "{rel}:{}: direct sysinfo::System construction",
                            i + 1
                        ));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "construct the metrics System only via sysinfo_budget::new_metrics_system():\n{}",
            offenders.join("\n")
        );
    }

    #[cfg(all(feature = "api", target_os = "linux"))]
    fn retained_proc_stat_fds() -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir("/proc/self/fd").expect("read /proc/self/fd") {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let t = target.to_string_lossy();
            let is_stat = t.starts_with("/proc/")
                && t.ends_with("/stat")
                && t.split('/')
                    .nth(2)
                    .is_some_and(|pid| pid.chars().all(|c| c.is_ascii_digit()));
            if is_stat {
                out.push(t.to_string());
            }
        }
        out
    }

    #[cfg(all(feature = "api", target_os = "linux"))]
    #[test]
    fn metrics_refresh_retains_no_proc_stat_fds() {
        let mut sys = super::new_metrics_system();
        super::refresh_metrics(&mut sys, true);
        super::refresh_metrics(&mut sys, true);
        assert!(!sys.processes().is_empty(), "processes were refreshed");
        let retained = retained_proc_stat_fds();
        assert!(
            retained.is_empty(),
            "sysinfo must not keep /proc stat handles open: {retained:?}"
        );
    }

    #[cfg(all(feature = "api", target_os = "linux"))]
    #[test]
    fn metrics_system_does_not_snapshot_processes_at_startup() {
        let sys = super::new_metrics_system();
        assert!(sys.processes().is_empty());
        assert!(retained_proc_stat_fds().is_empty());
    }

    #[cfg(feature = "api")]
    #[test]
    fn metrics_refresh_without_procs_leaves_process_table_empty() {
        let mut sys = super::new_metrics_system();
        super::refresh_metrics(&mut sys, false);
        assert!(sys.processes().is_empty());
        assert!(!sys.cpus().is_empty());
        assert!(sys.total_memory() > 0);
    }
}
