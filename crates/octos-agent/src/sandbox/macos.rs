//! macOS sandbox using sandbox-exec.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use super::{BLOCKED_ENV_VARS, DEFAULT_READ_ALLOW_PATHS, Sandbox};

/// Pick a scratch temp dir for a READ-ONLY workspace that is provably OUTSIDE
/// the (already-canonicalized) `real_cwd`.
///
/// `candidate` is the natural choice derived from `std::env::temp_dir()` (which
/// honours `$TMPDIR`). A hostile parent can set `$TMPDIR` to a path UNDER the
/// workspace — either an ABSOLUTE path inside it, or a RELATIVE path like "tmp"
/// that the sandboxed process resolves against its cwd to `<cwd>/tmp`. If we
/// used it we would (a) mutate the read-only workspace when creating the dir and
/// (b) grant SBPL write inside it — both defeat read-only (codex P2, rounds
/// 3+5). So we first ABSOLUTIZE the candidate against `real_cwd` (a relative
/// candidate is joined onto cwd, an absolute one is left as-is), then
/// canonicalize its location; if it falls inside `real_cwd` we fall back to a
/// base rooted at `/private/tmp` (the real path of `/tmp` on macOS, independent
/// of `$TMPDIR`) which is guaranteed outside any normal workspace. The returned
/// path is always ABSOLUTE. This function performs NO filesystem mutation — the
/// caller creates the dir only after the path is validated safe.
fn read_only_scratch_dir(candidate: &Path, real_cwd: &Path) -> std::path::PathBuf {
    // Absolutize the candidate FIRST. A hostile `$TMPDIR` can be RELATIVE (e.g.
    // "tmp" or "./sub/tmp"); the sandboxed process resolves it relative to its
    // cwd — i.e. it lands at `<cwd>/tmp`, inside the read-only workspace. If we
    // canonicalized it while still relative, the absolute `real_cwd`-contains
    // check below would MISS it (absolute vs relative never match) and we would
    // accept it, then `create_dir_all` would mutate the read-only workspace
    // (codex P2, round 5). Joining a relative candidate onto `real_cwd` models
    // where it truly resolves so the containment check rejects it. An absolute
    // candidate is unchanged by the join.
    let candidate_abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        real_cwd.join(candidate)
    };

    // Resolve the candidate to a real path. It usually does not exist yet, so
    // canonicalize the nearest existing ancestor and re-attach the tail.
    let candidate_real = canonicalize_lexical(&candidate_abs);

    if !candidate_real.starts_with(real_cwd) {
        // Return the ABSOLUTIZED candidate (never the raw relative one) so the
        // caller creates the dir at the location we actually validated.
        return candidate_abs;
    }

    // Candidate is inside the read-only workspace: fall back to a location that
    // does not depend on $TMPDIR. `/private/tmp` is the canonical macOS temp
    // root; guard against the pathological case where the workspace itself is
    // under it by only using it when it is outside cwd.
    let unique = format!("octos-sandbox-ro.{}", std::process::id());
    for base in ["/private/tmp", "/private/var/tmp"] {
        let fallback = Path::new(base).join(&unique);
        if !canonicalize_lexical(&fallback).starts_with(real_cwd) {
            return fallback;
        }
    }
    // Extremely unlikely: even the system temp roots are inside cwd. Return the
    // first fallback anyway — the SBPL grant is scoped to it, and the caller's
    // "outside cwd" invariant is enforced by the write-rule guard below.
    Path::new("/private/tmp").join(unique)
}

/// Canonicalize `path` as far as it exists, re-attaching any non-existent tail.
/// Used to decide containment for a scratch dir that has not been created yet
/// (`canonicalize` fails on non-existent paths, so we resolve the longest
/// existing ancestor and re-append the remaining components).
fn canonicalize_lexical(path: &Path) -> std::path::PathBuf {
    let mut ancestor = path;
    // Names peeled off while walking up, in leaf-to-root order.
    let mut peeled: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(mut real) = std::fs::canonicalize(ancestor) {
            for name in peeled.iter().rev() {
                real.push(name);
            }
            return real;
        }
        match ancestor.parent() {
            Some(parent) if parent != ancestor => {
                if let Some(name) = ancestor.file_name() {
                    peeled.push(name);
                }
                ancestor = parent;
            }
            // Reached the root without finding an existing ancestor.
            _ => return path.to_path_buf(),
        }
    }
}

/// #1976 — translate ONE v1 write-grant glob into an anchored SBPL regex
/// under the (canonical) workspace root. The translation is the EXACT mirror
/// of the tool-layer matcher (`globset` with `literal_separator`): `*` →
/// `[^/]*`, `?` → `[^/]`, everything else a regex-escaped literal. v1 grant
/// validation already rejected `**`/classes/alternations, so the two layers
/// provably grant the same path set.
/// SBPL-injection guard: true when `path` holds a byte that could break out
/// of a `(literal "…")` / `(subpath "…")` string or inject a rule. One
/// definition for every host-derived path interpolated into an SBPL
/// profile — a future tightening (e.g. rejecting more bytes) lands in one
/// place instead of the several ad-hoc copies scattered through this file.
pub(crate) fn path_has_sbpl_metachars(path: &str) -> bool {
    path.bytes()
        .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
}

pub(crate) fn glob_to_sbpl_regex(real_cwd: &str, glob: &str) -> String {
    let mut pattern = String::with_capacity(real_cwd.len() + glob.len() + 8);
    pattern.push('^');
    for ch in real_cwd.chars() {
        push_regex_escaped(&mut pattern, ch);
    }
    pattern.push('/');
    for ch in glob.chars() {
        match ch {
            '*' => pattern.push_str("[^/]*"),
            '?' => pattern.push_str("[^/]"),
            other => push_regex_escaped(&mut pattern, other),
        }
    }
    pattern.push('$');
    pattern
}

/// Escape a literal char for a POSIX-ERE SBPL regex.
fn push_regex_escaped(out: &mut String, ch: char) {
    if matches!(
        ch,
        '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\'
    ) {
        out.push('\\');
    }
    out.push(ch);
}

/// macOS sandbox using sandbox-exec.
#[derive(Clone)]
pub struct MacosSandbox {
    pub(crate) allow_network: bool,
    /// When non-empty, restrict file-read* to these paths + cwd.
    /// Empty = allow all reads (backward compatible).
    pub(crate) read_allow_paths: Vec<String>,
    pub(crate) strict_read_paths: bool,
    /// When `false`, the workspace cwd is NOT granted `file-write*`
    /// (read-only workspace for shell). `/dev/null` stays writable so
    /// shell redirections and git still work. Default constructions use
    /// `true` to preserve the historical writable-workspace behaviour.
    pub(crate) workspace_write: bool,
    /// TARGETED write to a repo's `.git` common dir (from a fleet worker's
    /// `FsGrant::Host`). `Some(<repo>/.git)` emits `(allow file-write* (subpath
    /// "<repo>/.git"))` ALONGSIDE the cwd grant — NOT a global `(allow
    /// file-write*)` (which would let a full-FS worker rewrite ANY host file,
    /// e.g. a LaunchAgent, ABOVE its grant) — so a Host-granted worktree worker's
    /// `git commit` can reach `<repo>/.git` (outside its cwd). Viable ONLY under
    /// an unrestricted-read profile (empty `read_allow_paths` → global
    /// `(allow file-read*)`), since git must also READ `<repo>/.git`; the pool
    /// gate (`supports_repo_git_write`) enforces that. Default `None` = today's
    /// cwd-only writable behaviour. The operator's explicit grant, NOT a fence.
    pub(crate) repo_git_write: Option<PathBuf>,
    /// Outer-loop #4 (docs/build-cache-pool.md §7.2) — the build-cache pool
    /// slot dir this session may READ and WRITE. Emitted as an INDEPENDENT
    /// pair of `(allow file-read* / file-write* (subpath "<slot>"))` rules
    /// (mirroring `external_tmp_write_rule`), NOT routed through
    /// `toolchain_write_grants`: those are appended only in the
    /// full-workspace-write arms, so a #1976 fence or a read-only workspace
    /// (deny-wins) would suppress the slot grant along with them and a
    /// fenced peer could never compile. The slot is harness-allocated
    /// infrastructure OUTSIDE the workspace; the fence constrains "what may
    /// change inside the workspace" and must not kill the compile-output dir
    /// outside it. Only the peer's OWN slot is ever set here (I4).
    pub(crate) build_cache_slot: Option<PathBuf>,
    /// #1976 — per-path WRITE fence: workspace-relative globs (`*`/`?` within
    /// one segment) naming the ONLY workspace paths the shell may write.
    /// `Some(globs)` REPLACES the broad cwd `file-write*` subpath grant with
    /// one `(allow file-write* (regex ...))` per glob (see
    /// [`glob_to_sbpl_regex`] — the translation mirrors the tool-layer
    /// globset semantics exactly), moves TMPDIR outside the workspace (the
    /// `<cwd>/tmp` scratch is no longer writable), and — deny-wins — narrows
    /// even `workspace_write=true`. An incoherent pairing with
    /// `repo_git_write` (validated impossible upstream: a fence excludes
    /// `FsGrant::Host`) resolves to the FENCE. macOS is the one backend that
    /// expresses the fence at the OS; what it CANNOT express is
    /// create-vs-overwrite, so `create_only`'s no-overwrite half remains
    /// tool-layer enforced (documented on `SandboxConfig::write_allow_globs`).
    pub(crate) write_allow_globs: Option<Vec<String>>,
    /// Precise toolchain write set (rustup settings/scratch, cargo caches —
    /// see `toolchain_write_grants` in `mod.rs` for what is and is NOT in
    /// it). Emitted ONLY alongside a full workspace write grant: a read-only
    /// workspace or a #1976 fence suppresses it (deny-wins — a profile that
    /// confines writes must not quietly regain toolchain caches).
    pub(crate) toolchain_write_grants: super::ToolchainWriteGrants,
    /// UPCR-2026-034 `read_parent` (see `SandboxConfig::read_only_view`).
    pub(crate) read_only_view: Option<Box<super::SandboxReadOnlyView>>,
}

/// Whether `path` would break out of an SBPL string literal.
fn has_sbpl_metachars(path: &str) -> bool {
    path.bytes()
        .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
}

/// The SBPL read rules of a request context's read-only view of its peer's
/// folder: read the folder, never an excluded (another context's) folder,
/// and read the cwd again (the context's own folder lies inside an excluded
/// one; the last matching rule wins). `None` when a path cannot be written
/// into SBPL safely (the caller refuses to run).
fn read_only_view_rules(view: &super::SandboxReadOnlyView, real_cwd: &str) -> Option<String> {
    let real = |path: &Path| {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .into_owned()
    };
    let root = real(&view.root);
    if has_sbpl_metachars(&root) {
        return None;
    }
    let mut rules = format!("(allow file-read* (subpath \"{root}\"))\n");
    for excluded in &view.excluded {
        let excluded = real(excluded);
        if has_sbpl_metachars(&excluded) {
            return None;
        }
        rules.push_str(&format!("(deny file-read* (subpath \"{excluded}\"))\n"));
    }
    rules.push_str(&format!("(allow file-read* (subpath \"{real_cwd}\"))\n"));
    Some(rules)
}

impl Sandbox for MacosSandbox {
    fn backend_name(&self) -> &'static str {
        "macos"
    }

    fn supports_repo_git_write(&self) -> bool {
        // A `(subpath "<repo>/.git")` rule grants the `.git` WRITE, but
        // `git commit` must also READ `<repo>/.git`. That read only holds under an
        // UNRESTRICTED-read profile (empty `read_allow_paths` → global
        // `(allow file-read*)`); a restricted-read profile would grant the write
        // but deny the read, so the worktree flow must fall back to scratch.
        self.read_allow_paths.is_empty()
    }

    fn runs_posix_sh(&self) -> bool {
        // `wrap_command` runs the shell string under sandbox-exec's `sh -c`,
        // as does its fail-closed error command.
        true
    }

    fn wrap_command_with_build_cache_slot(
        &self,
        shell_command: &str,
        cwd: &Path,
        slot: Option<&Path>,
    ) -> Command {
        // Shared session/profile backends must never retain a turn's slot.
        // None is authoritative too; legacy wrap_command keeps its config.
        let mut per_call = self.clone();
        per_call.build_cache_slot = slot.map(Path::to_path_buf);
        per_call.wrap_command(shell_command, cwd)
    }

    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        let cwd_str = cwd.to_string_lossy();

        // Reject paths with control characters or SBPL metacharacters to prevent
        // sandbox profile injection. Fail closed: error instead of running unsandboxed.
        if cwd_str
            .bytes()
            .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
        {
            tracing::error!("cwd contains SBPL metacharacters, refusing to execute");
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("echo 'sandbox error: cwd contains invalid characters' >&2; exit 1");
            return cmd;
        }

        // Path is validated above -- no escaping needed since \ and " are rejected.
        let cwd_escaped = &cwd_str;

        // #2136 review round 2, P1: `allow_network` stays AUTHORITATIVE.
        // A prior cut punched scoped egress for cargo when toolchains were
        // active — that silently overrode `allow_network=false` (and fleet
        // workers' network grants) for EVERY command, and the DNS hole did
        // not even resolve on macOS. Network is now exactly the flag: the
        // sandbox builds with CACHED dependencies (reads are allowed;
        // cargo's lock/index are writable), and fresh downloads require
        // `allow_network` (and, ultimately, proxy-isolated fetch).
        let network_rule = if self.allow_network {
            "(allow network*)"
        } else {
            "(deny network*)"
        };

        // Resolve cwd to its real path (macOS /tmp -> /private/tmp symlink).
        // SBPL subpath rules operate on real paths, so if cwd is /tmp/foo the
        // rule must use /private/tmp/foo or writes will be denied.
        let real_cwd = std::fs::canonicalize(cwd)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| cwd_escaped.to_string());
        // Validate the resolved path too
        if real_cwd
            .bytes()
            .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
        {
            tracing::error!("resolved cwd contains SBPL metacharacters, refusing to execute");
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("echo 'sandbox error: resolved cwd contains invalid characters' >&2; exit 1");
            return cmd;
        }

        // Build file-read rules: global if no read_allow_paths, restricted otherwise
        let read_rules = if self.read_allow_paths.is_empty() && !self.strict_read_paths {
            "(allow file-read*)".to_string()
        } else {
            let mut rules = Vec::new();
            // dyld needs to stat "/" during process startup (macOS Sequoia+).
            rules.push("(allow file-read* (literal \"/\"))".to_string());
            // Allow stat()/lstat() globally -- needed for getcwd(), realpath(),
            // and traversing parent directories of allowed subpaths. This only
            // permits metadata operations (file size, permissions, existence);
            // file-read-data (actual content reads) still requires subpath rules.
            rules.push("(allow file-read-metadata)".to_string());
            // Always allow reading the workspace (use canonical path for SBPL)
            rules.push(format!("(allow file-read* (subpath \"{real_cwd}\"))"));
            // Add configured read paths -- validate each for SBPL metacharacters
            // to prevent sandbox profile injection (same check as cwd above).
            for path in &self.read_allow_paths {
                if path.bytes().any(|b| {
                    b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"'
                }) {
                    tracing::error!(
                        path = %path,
                        "read_allow_paths entry contains SBPL metacharacters, skipping"
                    );
                    continue;
                }
                rules.push(format!("(allow file-read* (subpath \"{path}\"))"));
            }
            // Add default system paths
            if !self.strict_read_paths {
                for path in DEFAULT_READ_ALLOW_PATHS {
                    if !self.read_allow_paths.iter().any(|p| p == *path) && Path::new(path).exists() {
                        rules.push(format!("(allow file-read* (subpath \"{path}\"))"));
                    }
                }
            }
            rules.join("\n")
        };

        // UPCR-2026-034 `read_parent`: the peer's folder read-only, other
        // contexts' folders unreadable, in either read mode.
        let read_rules = match &self.read_only_view {
            None => read_rules,
            Some(view) => match read_only_view_rules(view, &real_cwd) {
                Some(view_rules) => format!("{read_rules}\n{view_rules}"),
                None => {
                    tracing::error!(
                        "read-only view path contains SBPL metacharacters, refusing to execute"
                    );
                    let mut cmd = Command::new("sh");
                    cmd.arg("-c").arg(
                        "echo 'sandbox error: read-only view path contains invalid characters' >&2; exit 1",
                    );
                    return cmd;
                }
            },
        };

        // Workspace write rule. Four cases:
        // - #1976 `write_allow_globs` (a per-path write fence): one
        //   `(allow file-write* (regex ...))` per granted glob REPLACES the
        //   broad cwd subpath grant — the shell can create/write ONLY the
        //   allowlisted paths, `(deny default)` refuses everything else in
        //   the workspace. Deny-wins over `workspace_write` AND over
        //   `repo_git_write` (that pairing is validated impossible upstream —
        //   a fence excludes `FsGrant::Host` — but if constructed anyway the
        //   FENCE is emitted). A glob with SBPL metacharacters (unreachable
        //   via a validated grant) is SKIPPED fail-closed, never injected.
        // - `repo_git_write` (a fleet worktree worker granted `FsGrant::Host`):
        //   grant `file-write*` to the cwd checkout AND a `(subpath "<repo>/.git")`
        //   so `git commit` can reach objects/refs/worktree-admin OUTSIDE the cwd.
        //   This is TARGETED, NOT a global `(allow file-write*)` — a full-FS
        //   worker must not be able to rewrite arbitrary host files (e.g. plant a
        //   LaunchAgent) ABOVE its grant. Only reached under unrestricted reads
        //   (see `supports_repo_git_write`), which the pool gate enforces. The
        //   `.git` path is validated for SBPL metacharacters; on failure it falls
        //   back to cwd-only write (fail closed — the commit fails, caught by the
        //   worker's branch-advance check, rather than injecting a rule).
        // - `workspace_write` (default): grant `file-write*` to the cwd subpath.
        // - neither (read-only profile): OMIT the grant so `(deny default)`
        //   denies the write. `/dev/null` stays writable regardless so shell
        //   redirections and git internals still function.
        let cwd_write_rule = format!("(allow file-write* (subpath \"{real_cwd}\"))\n");
        // Toolchain write rules (rustup settings/scratch, cargo caches).
        // Built here, appended ONLY in the full-workspace-write arms below:
        // under a #1976 fence or a read-only workspace the profile said
        // "confine writes", and deny-wins means these grants vanish with it.
        // Each path is validated for SBPL metacharacters like every other
        // injected path; an unsafe entry is skipped (that path simply stays
        // unwritable — fail closed), never emitted.
        let toolchain_write_rules = {
            let mut rules = String::new();
            for path in &self.toolchain_write_grants.literals {
                if path_has_sbpl_metachars(path) {
                    tracing::error!(path = %path, "toolchain grant contains SBPL metacharacters, skipping");
                    continue;
                }
                rules.push_str(&format!("(allow file-write* (literal \"{path}\"))\n"));
            }
            for path in &self.toolchain_write_grants.subpaths {
                if path_has_sbpl_metachars(path) {
                    tracing::error!(path = %path, "toolchain grant contains SBPL metacharacters, skipping");
                    continue;
                }
                rules.push_str(&format!("(allow file-write* (subpath \"{path}\"))\n"));
            }
            rules
        };
        let workspace_write_rule = if let Some(globs) = &self.write_allow_globs {
            let mut rules = String::new();
            for glob in globs {
                if glob.bytes().any(|b| {
                    b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"'
                }) {
                    tracing::error!(
                        glob = %glob,
                        "write_allow_globs entry contains SBPL metacharacters, skipping \
                         (the path is simply not writable — fail closed)"
                    );
                    continue;
                }
                rules.push_str(&format!(
                    "(allow file-write* (regex #\"{}\"))\n",
                    glob_to_sbpl_regex(&real_cwd, glob)
                ));
            }
            if self.repo_git_write.is_some() {
                tracing::error!(
                    "write_allow_globs and repo_git_write are mutually exclusive (a fence \
                     excludes FsGrant::Host); emitting the FENCE only (deny-wins)"
                );
            }
            rules
        } else if let Some(git_dir) = &self.repo_git_write {
            let real_git = std::fs::canonicalize(git_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| git_dir.to_string_lossy().to_string());
            if real_git
                .bytes()
                .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
            {
                tracing::error!(
                    "repo_git_write path contains SBPL metacharacters, granting cwd-only write"
                );
                format!("{cwd_write_rule}{toolchain_write_rules}")
            } else {
                format!(
                    "{cwd_write_rule}(allow file-write* (subpath \"{real_git}\"))\n{toolchain_write_rules}"
                )
            }
        } else if self.workspace_write {
            format!("{cwd_write_rule}{toolchain_write_rules}")
        } else {
            String::new()
        };

        // #1976: a fenced workspace is read-only OUTSIDE the granted globs, so
        // `<cwd>/tmp` is not writable — scratch must live outside, exactly
        // like the read-only profile.
        let workspace_scratch_writable = self.workspace_write && self.write_allow_globs.is_none();

        // Choose a scratch temp dir for TMPDIR/TEMP/TMP.
        //
        // - Writable workspace: keep the historical `<cwd>/tmp` (covered by the
        //   cwd file-write* grant above).
        // - Read-only OR fenced workspace (P2, codex; #1976): NEVER create or
        //   point TMPDIR under the workspace — that would mutate it BEFORE
        //   sandbox-exec even runs. Use a private dir OUTSIDE the workspace and
        //   grant SBPL write to THAT instead, so read-only truly means no
        //   workspace mutation while tools that need scratch space (Python
        //   tempfile, compilers) still work.
        let user_tmp = if workspace_scratch_writable {
            cwd.join("tmp")
        } else {
            // `std::env::temp_dir()` honours `$TMPDIR`, which a hostile parent
            // could point UNDER the workspace. Route the candidate through
            // `read_only_scratch_dir`, which guarantees a path OUTSIDE the
            // (canonicalized) workspace so we never mutate it (codex P2).
            let candidate =
                std::env::temp_dir().join(format!("octos-sandbox-ro.{}", std::process::id()));
            let real_cwd_path = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            read_only_scratch_dir(&candidate, &real_cwd_path)
        };
        // Create the scratch dir. For the writable case this lives inside the
        // (writable) workspace; for the read-only case it is outside it.
        let _ = std::fs::create_dir_all(&user_tmp);

        // For the read-only / fenced case, grant SBPL file-write* to the
        // external temp dir's real path so scratch writes succeed there (not
        // in the workspace). The path is validated for SBPL metacharacters;
        // if it is unexpectedly unsafe we simply omit the grant (fail-closed:
        // scratch writes fail rather than the profile being injectable).
        let external_tmp_write_rule = if workspace_scratch_writable {
            String::new()
        } else {
            let real_tmp_path =
                std::fs::canonicalize(&user_tmp).unwrap_or_else(|_| user_tmp.clone());
            let real_tmp = real_tmp_path.to_string_lossy().to_string();
            // Defence in depth: NEVER grant write to a temp path that resolved
            // inside the workspace — that would re-open the read-only hole even
            // though `read_only_scratch_dir` already steers outside cwd
            // (codex P2, fail-closed).
            let real_cwd_path = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            if real_tmp_path.starts_with(&real_cwd_path) {
                tracing::error!(
                    "external temp dir resolved inside the read-only workspace, omitting write grant"
                );
                String::new()
            } else if real_tmp
                .bytes()
                .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
            {
                tracing::error!(
                    "external temp dir contains SBPL metacharacters, omitting write grant"
                );
                String::new()
            } else {
                format!("(allow file-write* (subpath \"{real_tmp}\"))\n")
            }
        };

        // Outer-loop #4 (§7.2, candidate B): the build-cache slot gets its
        // OWN read+write subpath allow, mirroring `external_tmp_write_rule`
        // — independent of the workspace arms (`workspace_write`,
        // `write_allow_globs`, toolchain grants) so a fence or read-only
        // profile cannot suppress it (deny-wins would otherwise leave a
        // fenced peer unable to compile at all). `file-read*` is emitted
        // alongside `file-write*` because the pool may live outside the
        // octos home when `data_dir` is overridden, and a restricted-read
        // profile would otherwise grant the write but deny the read. The
        // path is canonicalized (SBPL subpath rules match real paths) and
        // validated for SBPL metacharacters; an unsafe path is SKIPPED, not
        // emitted — fail-closed: the compile fails visibly rather than the
        // profile being injectable.
        let build_cache_slot_rules = match &self.build_cache_slot {
            Some(slot) => {
                let real_slot_path = std::fs::canonicalize(slot).unwrap_or_else(|_| slot.clone());
                let real_slot = real_slot_path.to_string_lossy().to_string();
                if path_has_sbpl_metachars(&real_slot) {
                    tracing::error!(
                        slot = %real_slot,
                        "build_cache_slot contains SBPL metacharacters, skipping grant \
                         (the slot stays unwritable — fail closed)"
                    );
                    String::new()
                } else {
                    format!(
                        "(allow file-read* (subpath \"{real_slot}\"))\n\
                         (allow file-write* (subpath \"{real_slot}/target\"))\n"
                    )
                }
            }
            None => String::new(),
        };

        let profile = format!(
            r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow process-info*)
(allow sysctl-read)
(allow mach-lookup)
(allow mach-register)
(allow ipc-posix*)
(allow signal)
(allow file-ioctl)
{read_rules}
(allow file-write* (literal "/dev/null"))
{workspace_write_rule}{external_tmp_write_rule}{build_cache_slot_rules}{network_rule}
"#,
        );

        let mut cmd = Command::new("sandbox-exec");
        // Redirect TMPDIR/TEMP/TMP to the chosen scratch dir (inside cwd when
        // writable, outside the workspace when read-only).
        cmd.env("TMPDIR", &user_tmp);
        cmd.env("TEMP", &user_tmp);
        cmd.env("TMP", &user_tmp);
        // No CARGO_HOME redirect (#2136 review round 2, P2): a
        // <cwd>/tmp/cargo-home overlay polluted non-git-ignored repos,
        // fragmented the cache per workdir, and hid host cargo config +
        // credentials. Host CARGO_HOME stands; cached deps are read from
        // it (reads allowed), and only the non-executable lock/index are
        // writable (see toolchain_write_grants).
        // Clear dangerous environment variables (sandbox-exec inherits parent env)
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.arg("-p")
            .arg(profile)
            .arg("sh")
            .arg("-c")
            .arg(shell_command)
            .current_dir(cwd);
        cmd
    }
}
