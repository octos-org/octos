//! Canonical config/auth/data path resolver — the single source of truth.
//!
//! Every site that needs to know *where* octos reads/writes user config, auth
//! credentials, or runtime state MUST resolve through [`resolve_config_context`]
//! and consume the returned [`ConfigContext`]. No call site recomputes these
//! paths from the environment on its own — that split-brain divergence was the
//! root cause of the two prior failed attempts at XDG-primary config.
//!
//! # Resolution rules (codex-vetted)
//!
//! Inputs: the `--data-dir` CLI flag plus the `OCTOS_HOME` and
//! `OCTOS_CONFIG_DIR` environment variables. Empty-string env vars are treated
//! as unset. `OCTOS_HOME` is normalized (tilde-expanded, made absolute,
//! `canonicalize`d best-effort) before being compared to the default `~/.octos`.
//!
//! * `data_dir` = `--data-dir` > `$OCTOS_HOME` > `~/.octos`
//!   (state/sessions/skills — unchanged semantics).
//! * `is_explicit` = `--data-dir` set OR `OCTOS_CONFIG_DIR` set OR
//!   (`OCTOS_HOME` set AND its normalized form != `~/.octos`).
//!   `is_default = !is_explicit`.
//! * `config_home` = `OCTOS_CONFIG_DIR` if set, else `data_dir` when explicit,
//!   else the XDG default (`${XDG_CONFIG_HOME:-~/.config}/octos` on macOS+Linux,
//!   `%APPDATA%\octos` on Windows — see `xdg_config_home`).
//! * `auth_home` = `OCTOS_CONFIG_DIR` if set, else the same XDG default.
//!   **DECOUPLED from `data_dir`** so per-profile
//!   gateways (which run with `--data-dir <profile-data>`) keep the host's
//!   shared, global `octos auth login`.

use std::path::{Path, PathBuf};

/// Resolved, canonical paths for config / auth / runtime state.
///
/// Constructed exactly once per command entrypoint via
/// [`resolve_config_context`] and threaded to every consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigContext {
    /// Where `config.json` is read from / written to (when not project-local).
    pub config_home: PathBuf,
    /// Where `auth.json` lives. GLOBAL (XDG) unless `OCTOS_CONFIG_DIR` is set.
    pub auth_home: PathBuf,
    /// Runtime state root: episodes, sessions, skills, memory, logs.
    pub data_dir: PathBuf,
    /// `true` when no explicit override was supplied (default install). When
    /// `true`, config load may fall back to the legacy `~/.octos/config.json`
    /// and config migration into XDG is permitted.
    pub is_default: bool,
}

/// Read an environment variable, treating empty strings as unset.
fn env_nonempty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Best-effort path normalization for comparison: expand a leading `~`, make
/// the path absolute relative to `$HOME`/CWD, then `canonicalize` if the path
/// exists on disk. Falls back to the lexical form when canonicalization fails
/// (e.g. the directory does not exist yet).
fn normalize_for_compare(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(&expanded)
    } else {
        expanded
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

/// Normalize a path for RETURN (config_home / auth_home / data_dir): expand a
/// leading `~` and make it absolute, but do NOT `canonicalize`. Canonicalize is
/// reserved for the `is_default` *comparison* — applying it to returned paths
/// would resolve symlinks (e.g. macOS `/var`→`/private/var`), require the path
/// to already exist, and surprise callers/tests with a rewritten prefix. We
/// only need to guarantee no literal `~`/relative segment leaks into a file
/// path.
fn normalize_for_return(path: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(&expanded)
    } else {
        expanded
    }
}

/// Expand a leading `~` or `~/` against `$HOME`.
fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

/// The default runtime data dir: `~/.octos`.
fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".octos"))
        .unwrap_or_else(|| PathBuf::from(".octos"))
}

/// The default config home for octos:
/// * Unix (macOS **and** Linux): `${XDG_CONFIG_HOME:-~/.config}/octos`. We use
///   true XDG `~/.config` on macOS too — NOT Apple's `~/Library/Application
///   Support` — because octos is a CLI and that's the prevailing CLI convention
///   (gh, nvim, helix, …); it's discoverable and matches Linux for one mental
///   model.
/// * Windows: `%APPDATA%\octos` (via `dirs::config_dir()`), the correct per-user
///   config home there (no `~/.config` convention on Windows).
///
/// Falls back to `~/.octos` only if the home/config dir can't be determined.
fn xdg_config_home() -> PathBuf {
    #[cfg(windows)]
    {
        dirs::config_dir()
            .map(|d| d.join("octos"))
            .unwrap_or_else(default_data_dir)
    }
    #[cfg(not(windows))]
    {
        // $XDG_CONFIG_HOME wins when set to an ABSOLUTE path (the spec ignores
        // relative values); else ~/.config.
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            let p = PathBuf::from(xdg);
            if p.is_absolute() {
                return p.join("octos");
            }
        }
        dirs::home_dir()
            .map(|h| h.join(".config").join("octos"))
            .unwrap_or_else(default_data_dir)
    }
}

/// Resolve the canonical [`ConfigContext`] from the `--data-dir` flag plus the
/// `OCTOS_HOME` / `OCTOS_CONFIG_DIR` environment variables.
///
/// See the module docs for the full rule set. This is the ONLY function that
/// reads those env vars for path resolution.
pub fn resolve_config_context(cli_data_dir: Option<&Path>) -> ConfigContext {
    let octos_home = env_nonempty("OCTOS_HOME").map(PathBuf::from);
    let octos_config_dir = env_nonempty("OCTOS_CONFIG_DIR").map(PathBuf::from);

    // data_dir: --data-dir > $OCTOS_HOME > ~/.octos
    let data_dir = cli_data_dir
        .map(PathBuf::from)
        .or_else(|| octos_home.clone())
        .unwrap_or_else(default_data_dir);

    // Is OCTOS_HOME a non-default override? Compare normalized paths so
    // `OCTOS_HOME=~/.octos`, `OCTOS_HOME=$HOME/.octos`, and a trailing-slash
    // variant all resolve to the default (no split-brain).
    let octos_home_is_nondefault = octos_home
        .as_deref()
        .map(|h| normalize_for_compare(h) != normalize_for_compare(&default_data_dir()))
        .unwrap_or(false);

    let is_explicit =
        cli_data_dir.is_some() || octos_config_dir.is_some() || octos_home_is_nondefault;
    let is_default = !is_explicit;

    let config_home = if let Some(ref cfg) = octos_config_dir {
        cfg.clone()
    } else if is_explicit {
        data_dir.clone()
    } else {
        xdg_config_home()
    };

    // auth_home is GLOBAL: OCTOS_CONFIG_DIR if set, else XDG. NEVER data_dir.
    let auth_home = octos_config_dir.clone().unwrap_or_else(xdg_config_home);

    // Normalize the RETURNED paths (expand a leading `~`, make absolute,
    // canonicalize-if-it-exists). Without this, an env value like
    // `OCTOS_HOME=~/x` or a relative `--data-dir foo` would propagate a literal
    // `~`/relative segment into the config/auth/state file paths.
    ConfigContext {
        config_home: normalize_for_return(&config_home),
        auth_home: normalize_for_return(&auth_home),
        data_dir: normalize_for_return(&data_dir),
        is_default,
    }
}

/// Atomically copy `src` into `dest`, non-destructively and idempotently.
///
/// Semantics (shared by config + auth migration):
/// * No-op (returns `false`) if `dest` already exists or `src` is missing —
///   we never overwrite a destination or invent a source.
/// * `mkdir -p` the destination's parent directory.
/// * Write into a temp file *in the destination directory*, `fsync` it, set
///   `mode` (Unix only) on the temp file before the rename so the final file
///   never exists with looser permissions, then `fs::rename` (atomic on the
///   same filesystem).
/// * Best-effort: any I/O error is swallowed and reported as `false` (the
///   legacy file is always left intact, so a failed migration degrades to
///   "config/auth still read from legacy" rather than data loss).
///
/// Returns `true` when a copy was performed.
pub fn atomic_copy_into(src: &Path, dest: &Path, mode: Option<u32>) -> bool {
    if dest.exists() || !src.exists() {
        return false;
    }
    let Some(parent) = dest.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let Ok(bytes) = std::fs::read(src) else {
        return false;
    };

    // Unpredictable temp name in the destination directory (same filesystem →
    // atomic rename). Combining pid, a monotonic counter, and the nanosecond
    // clock keeps the name from being guessable so an attacker cannot
    // pre-plant a file/symlink that we would open-and-truncate.
    let tmp = parent.join(format!(
        ".{}.{}.{}.{}.tmp",
        dest.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "octos-migrate".to_string()),
        std::process::id(),
        next_temp_counter(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));

    let write_ok = write_temp(&tmp, &bytes, mode);
    if !write_ok {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }

    // `fs::rename` overwrites an existing dest on Unix. We already established
    // `!dest.exists()` above and migration runs once at startup, so the
    // remaining window is negligible; keep the rename (atomic same-fs swap).
    if std::fs::rename(&tmp, dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Monotonic per-process counter for temp-file name uniqueness.
fn next_temp_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Write `bytes` to a FRESH temp file (`create_new` — never opens an existing
/// path, so a pre-planted file/symlink at `tmp` causes a clean failure rather
/// than a truncate-and-write), applying `mode` on Unix, and fsync before close.
fn write_temp(tmp: &Path, bytes: &[u8], mode: Option<u32>) -> bool {
    use std::io::Write as _;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut opts = std::fs::OpenOptions::new();
        // create_new(true) ⇒ O_CREAT|O_EXCL: fails if `tmp` already exists,
        // and applies `mode` atomically at creation (no looser-perm window).
        opts.write(true).create_new(true);
        if let Some(m) = mode {
            opts.mode(m);
        }
        let Ok(mut f) = opts.open(tmp) else {
            return false;
        };
        if f.write_all(bytes).is_err() {
            return false;
        }
        // Defense in depth: re-assert the mode after creation in case a umask
        // or platform quirk loosened it. Failure here aborts the copy.
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt as _;
            if std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(m)).is_err() {
                return false;
            }
        }
        f.sync_all().is_ok()
    }

    #[cfg(not(unix))]
    {
        let _ = mode;
        let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tmp)
        else {
            return false;
        };
        if f.write_all(bytes).is_err() {
            return false;
        }
        f.sync_all().is_ok()
    }
}

/// Run the config + auth migrations for the given context. Idempotent and
/// best-effort. Call once at the command entrypoint.
///
/// * Config migration: only when `ctx.is_default`. Copies legacy
///   `~/.octos/config.json` → `ctx.config_home/config.json` (the XDG default).
///   Logs a one-line notice when the copy happens so the user knows their
///   config now lives at the XDG path. Legacy file is left intact.
/// * Auth migration: only when `ctx.auth_home` is the XDG default (i.e.
///   `OCTOS_CONFIG_DIR` is unset). Copies legacy `~/.octos/auth.json` →
///   `ctx.auth_home/auth.json` with mode `0600`. Never migrates host auth into
///   an `OCTOS_CONFIG_DIR` tenant directory. Legacy file is left intact.
pub fn run_migrations(ctx: &ConfigContext) {
    let legacy_root = default_data_dir();

    // `auth_home`/config XDG-default gating is derived from the resolved
    // context — NOT re-read from the environment — so the migration can never
    // diverge from what the resolver decided. `auth_home == xdg_config_home()`
    // holds exactly when `OCTOS_CONFIG_DIR` was unset, which is the only case
    // in which we touch the XDG location.
    let xdg = xdg_config_home();
    let auth_home_is_xdg_default = ctx.auth_home == xdg;

    // ── Config migration (default installs only) ──────────────────────
    if ctx.is_default {
        let legacy_config = legacy_root.join("config.json");
        let dest_config = ctx.config_home.join("config.json");
        if legacy_config != dest_config && atomic_copy_into(&legacy_config, &dest_config, None) {
            tracing::info!(
                from = %legacy_config.display(),
                to = %dest_config.display(),
                "migrated config.json to the XDG config directory (legacy copy left intact)"
            );
        }
    } else if auth_home_is_xdg_default {
        // Non-default via OCTOS_HOME (not OCTOS_CONFIG_DIR): config now lives in
        // the state dir. Surface a one-line notice so an
        // `OCTOS_HOME=~/projects/foo` user is not surprised their config lives
        // there. No copy — explicit dirs are isolated; this is informational.
        tracing::info!(
            config_home = %ctx.config_home.display(),
            "OCTOS_HOME override active: config.json is read/written under the state dir"
        );
    }

    // ── Auth migration (XDG-default auth_home only) ───────────────────
    if auth_home_is_xdg_default {
        let legacy_auth = legacy_root.join("auth.json");
        let dest_auth = ctx.auth_home.join("auth.json");
        if legacy_auth != dest_auth && atomic_copy_into(&legacy_auth, &dest_auth, Some(0o600)) {
            tracing::info!(
                from = %legacy_auth.display(),
                to = %dest_auth.display(),
                "migrated auth.json to the XDG config directory (mode 0600, legacy copy left intact)"
            );
        }
    }
}

/// Process-wide lock for tests that mutate the global `HOME` / `OCTOS_HOME` /
/// `OCTOS_CONFIG_DIR` environment variables. These vars are process-global, so
/// EVERY env-pivoting test in the crate (here and in `config.rs`) must
/// serialize against this single mutex — separate per-module locks would let
/// tests in different modules race each other and flake.
#[cfg(any(test, feature = "test-util"))]
pub static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Alias for the crate-wide env lock (see [`super::TEST_ENV_LOCK`]).
    use super::TEST_ENV_LOCK as ENV_LOCK;

    struct EnvGuard {
        home: Option<std::ffi::OsString>,
        octos_home: Option<std::ffi::OsString>,
        octos_config_dir: Option<std::ffi::OsString>,
        xdg_config_home: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        #[allow(unsafe_code)]
        fn pivot(home: &Path) -> Self {
            let g = EnvGuard {
                home: std::env::var_os("HOME"),
                octos_home: std::env::var_os("OCTOS_HOME"),
                octos_config_dir: std::env::var_os("OCTOS_CONFIG_DIR"),
                xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            };
            // SAFETY: callers hold ENV_LOCK; restored in Drop.
            unsafe {
                std::env::set_var("HOME", home);
                std::env::remove_var("OCTOS_HOME");
                std::env::remove_var("OCTOS_CONFIG_DIR");
                std::env::remove_var("XDG_CONFIG_HOME");
            }
            g
        }
    }

    impl Drop for EnvGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: callers hold ENV_LOCK for the guard's lifetime.
            unsafe {
                restore("HOME", &self.home);
                restore("OCTOS_HOME", &self.octos_home);
                restore("OCTOS_CONFIG_DIR", &self.octos_config_dir);
                restore("XDG_CONFIG_HOME", &self.xdg_config_home);
            }
        }
    }

    #[allow(unsafe_code)]
    unsafe fn restore(key: &str, val: &Option<std::ffi::OsString>) {
        match val {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[allow(unsafe_code)]
    fn set_env(key: &str, val: &str) {
        // SAFETY: callers hold ENV_LOCK.
        unsafe { std::env::set_var(key, val) };
    }

    /// Gate 4 + 10: `OCTOS_HOME == ~/.octos` (and the empty-string case) are
    /// treated as the default — no split-brain, config_home is XDG.
    #[test]
    fn octos_home_equal_to_default_is_treated_as_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let _env = EnvGuard::pivot(home);

        // OCTOS_HOME explicitly set to the default location.
        set_env("OCTOS_HOME", home.join(".octos").to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert!(ctx.is_default, "OCTOS_HOME==~/.octos must be is_default");
        assert_eq!(
            ctx.config_home,
            xdg_config_home(),
            "OCTOS_HOME==~/.octos must resolve config_home to XDG (no split-brain)"
        );
        assert_eq!(ctx.data_dir, home.join(".octos"));
    }

    /// Gate 10: empty-string OCTOS_HOME is treated as unset → default.
    #[test]
    fn empty_octos_home_is_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path());
        set_env("OCTOS_HOME", "");
        let ctx = resolve_config_context(None);
        assert!(ctx.is_default);
        assert_eq!(ctx.data_dir, tmp.path().join(".octos"));
        assert_eq!(ctx.config_home, xdg_config_home());
    }

    /// Default config_home on unix is true XDG `~/.config/octos` — NOT Apple's
    /// `~/Library/Application Support` — and honours an absolute $XDG_CONFIG_HOME.
    #[test]
    #[cfg(not(windows))]
    fn unix_default_config_home_is_dot_config_not_app_support() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path()); // clears XDG_CONFIG_HOME

        // No XDG_CONFIG_HOME → ~/.config/octos.
        let ctx = resolve_config_context(None);
        assert_eq!(ctx.config_home, tmp.path().join(".config").join("octos"));
        assert_eq!(ctx.auth_home, tmp.path().join(".config").join("octos"));
        assert!(
            !ctx.config_home
                .to_string_lossy()
                .contains("Application Support"),
            "config must not live in ~/Library/Application Support, got {}",
            ctx.config_home.display()
        );

        // Absolute XDG_CONFIG_HOME wins.
        let xdg = tmp.path().join("xdgcfg");
        set_env("XDG_CONFIG_HOME", xdg.to_str().unwrap());
        let ctx2 = resolve_config_context(None);
        assert_eq!(ctx2.config_home, xdg.join("octos"));
    }

    /// Gate 5: non-default OCTOS_HOME → config_home == that state dir.
    #[test]
    fn nondefault_octos_home_sets_config_home_to_state_dir() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path());
        let custom = tmp.path().join("projects").join("foo");
        set_env("OCTOS_HOME", custom.to_str().unwrap());
        let ctx = resolve_config_context(None);
        assert!(!ctx.is_default);
        assert_eq!(ctx.data_dir, custom);
        assert_eq!(ctx.config_home, custom);
        // auth stays GLOBAL.
        assert_eq!(ctx.auth_home, xdg_config_home());
    }

    /// Gate 3 + 7: `--data-dir T` → config_home == T, but auth_home stays the
    /// GLOBAL XDG default (shared login across per-profile gateways).
    #[test]
    fn cli_data_dir_isolates_config_but_auth_stays_global() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path());
        let t = tmp.path().join("profile-data");
        let ctx = resolve_config_context(Some(&t));
        assert!(!ctx.is_default);
        assert_eq!(ctx.data_dir, t);
        assert_eq!(ctx.config_home, t, "explicit --data-dir → config in T");
        assert_eq!(
            ctx.auth_home,
            xdg_config_home(),
            "auth MUST stay global (XDG) across --data-dir profiles"
        );
    }

    /// Gate 6: `OCTOS_CONFIG_DIR=C` → BOTH config and auth resolve under C.
    #[test]
    fn octos_config_dir_governs_both_config_and_auth() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path());
        let c = tmp.path().join("tenant-config");
        set_env("OCTOS_CONFIG_DIR", c.to_str().unwrap());
        // Even with a --data-dir, OCTOS_CONFIG_DIR wins for config + auth.
        let t = tmp.path().join("tenant-data");
        let ctx = resolve_config_context(Some(&t));
        assert!(!ctx.is_default);
        assert_eq!(ctx.data_dir, t);
        assert_eq!(ctx.config_home, c, "OCTOS_CONFIG_DIR governs config_home");
        assert_eq!(ctx.auth_home, c, "OCTOS_CONFIG_DIR governs auth_home");
    }

    /// Gate 1/2 baseline: no env, no flag → default install. data_dir is
    /// ~/.octos, config_home is XDG, auth_home is XDG, is_default true.
    #[test]
    fn pure_default_resolves_to_xdg_config_and_legacy_data() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path());
        let ctx = resolve_config_context(None);
        assert!(ctx.is_default);
        assert_eq!(ctx.data_dir, tmp.path().join(".octos"));
        assert_eq!(ctx.config_home, xdg_config_home());
        assert_eq!(ctx.auth_home, xdg_config_home());
    }

    // ── atomic_copy_into ──────────────────────────────────────────────

    #[test]
    fn atomic_copy_into_copies_when_dest_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.json");
        let dest = tmp.path().join("nested").join("dest.json");
        std::fs::write(&src, b"{\"k\":1}").unwrap();

        assert!(atomic_copy_into(&src, &dest, None));
        assert_eq!(std::fs::read(&dest).unwrap(), b"{\"k\":1}");
        // src left intact.
        assert!(src.exists());
    }

    #[test]
    fn atomic_copy_into_is_idempotent_and_nondestructive() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.json");
        let dest = tmp.path().join("dest.json");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dest, b"existing").unwrap();

        // Dest exists → no copy, dest preserved.
        assert!(!atomic_copy_into(&src, &dest, None));
        assert_eq!(std::fs::read(&dest).unwrap(), b"existing");
    }

    #[test]
    fn atomic_copy_into_noop_when_src_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("absent.json");
        let dest = tmp.path().join("dest.json");
        assert!(!atomic_copy_into(&src, &dest, None));
        assert!(!dest.exists());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_copy_into_applies_0600_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.json");
        let dest = tmp.path().join("dest.json");
        // Simulate a world-readable legacy file (0644).
        std::fs::write(&src, b"secret").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(atomic_copy_into(&src, &dest, Some(0o600)));
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "dest must be 0600 regardless of src perms");
    }

    // ── run_migrations (gates 8 + 9) ──────────────────────────────────

    /// Gate 9: default install + legacy ~/.octos/config.json + no XDG →
    /// XDG config created, legacy left intact.
    #[test]
    fn config_migration_copies_legacy_to_xdg_and_leaves_legacy_intact() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path());

        let legacy = tmp.path().join(".octos").join("config.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"{\"provider\":\"anthropic\"}").unwrap();

        let ctx = resolve_config_context(None);
        run_migrations(&ctx);

        let xdg = ctx.config_home.join("config.json");
        assert!(xdg.exists(), "XDG config.json must be created");
        assert_eq!(
            std::fs::read(&xdg).unwrap(),
            b"{\"provider\":\"anthropic\"}"
        );
        assert!(legacy.exists(), "legacy config.json must be left intact");
    }

    /// Gate 8: legacy ~/.octos/auth.json (0644) → XDG auth.json at 0600;
    /// legacy left intact.
    #[cfg(unix)]
    #[test]
    fn auth_migration_produces_0600_and_leaves_legacy_intact() {
        use std::os::unix::fs::PermissionsExt as _;
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path());

        let legacy = tmp.path().join(".octos").join("auth.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"{\"credentials\":{}}").unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o644)).unwrap();

        let ctx = resolve_config_context(None);
        run_migrations(&ctx);

        let xdg = ctx.auth_home.join("auth.json");
        assert!(xdg.exists(), "XDG auth.json must be created");
        let mode = std::fs::metadata(&xdg).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "migrated auth.json must be 0600");
        assert!(legacy.exists(), "legacy auth.json must be left intact");
    }

    /// Gate 8 (tenant): OCTOS_CONFIG_DIR set → host auth is NOT migrated into
    /// the tenant dir.
    #[test]
    fn auth_migration_does_not_touch_tenant_config_dir() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::pivot(tmp.path());

        let legacy = tmp.path().join(".octos").join("auth.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"{\"credentials\":{}}").unwrap();

        let tenant = tmp.path().join("tenant");
        set_env("OCTOS_CONFIG_DIR", tenant.to_str().unwrap());

        let ctx = resolve_config_context(None);
        run_migrations(&ctx);

        assert!(
            !tenant.join("auth.json").exists(),
            "host auth must NOT be migrated into an OCTOS_CONFIG_DIR tenant dir"
        );
        assert!(legacy.exists(), "legacy auth.json must be left intact");
    }
}
