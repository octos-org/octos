//! # SessionScope — the single filesystem contract for octos components
//!
//! Every component in octos that touches the filesystem on behalf of a
//! user session — pipeline workers, plugin tools, file tools, sandboxes,
//! shell, spawn — MUST derive its working directory and validate its
//! file paths against the [`SessionScope`] for that session. No
//! component computes its own working directory from raw inputs like
//! `data_dir`, `profile_id`, or environment variables.
//!
//! This module defines that contract. It contains **only types** and
//! constructors that validate consistency; it does not change runtime
//! behaviour. Migrations onto the contract land in subsequent PRs.
//!
//! ## Why this exists
//!
//! Today (2026-05-23) octos has three separate places computing a
//! session-or-tenant CWD: `chat.rs` for solo, `serve.rs`/`handlers.rs`
//! for the AppUI/serve path, and an ad-hoc `working_dir: PathBuf`
//! pinned at construction time inside `RunPipelineTool`. Plugins
//! (mofa-podcast, mofa-research, etc.) make their own `current_dir`
//! choices. The five-round PR #1186 path-traversal saga, PR #1189
//! workspace-root rescue, and PR #1192/#1195/#1197 memory-contamination
//! cascade are all symptoms of the missing contract: each new fix only
//! patched the one component that surfaced a bug, leaving the next one
//! exposed.
//!
//! Empirically observed bugs that a single contract eliminates:
//! - cross-session contamination: a pipeline worker spawned at
//!   `<profile>/data/` (the profile root, not a per-session dir) sees
//!   every prior session's `*.md` and calls `read_file` on them
//!   instead of running `web_search`. Fleet evidence: mini5 JWST
//!   prompt produced an Intel/Tim Cook/GPT-5.5 verification report
//!   because plan_and_search workers read stale Apr 25 research dirs.
//! - path-translation asymmetry: `write_file` writes to the workspace
//!   root, `podcast_generate` runs in a `skill-output/` subdir;
//!   without a shared scope, the resolver must implement bespoke
//!   "probe one level up" rescues per-plugin.
//! - traversal hardening drift: each new plugin arg with a path needs
//!   its own `has_unsafe_components` check; one missed key reopens
//!   the escape.
//!
//! ## The two scope modes
//!
//! Octos runs in two modes with different isolation contracts:
//!
//! **Multi-tenant** (`octos serve` + AppUI web client):
//! - Multiple tenants share one octos process. Each tenant has its
//!   own profile directory at `<config_dir>/profiles/<tenant_id>/`.
//! - Within a tenant, multiple concurrent sessions share long-lived
//!   state (skill installs, optionally research cache) but each
//!   session has its own ephemeral workspace.
//! - Boundaries enforced: cross-tenant access refused unconditionally;
//!   cross-session writes refused at the workspace layer; reads of
//!   cross-session content require explicit user action (`/resume`,
//!   `recall` tool, etc.) not implicit CWD scan.
//!
//! **Solo** (`octos chat` invoked by a developer in a terminal):
//! - One user, one process, one persistent CWD chosen by the user
//!   (or `--cwd` flag). Mirrors Claude Code's model: the user opens
//!   a project directory and works there across sessions.
//! - No tenant boundary. Session and workspace collapse to the
//!   user-chosen CWD. Cross-session continuity is a feature, not a
//!   bug.
//! - Permission grants extend the scope (analogous to Claude Code's
//!   per-Edit/Write approval): the user can grant access to dirs
//!   outside CWD case-by-case.
//!
//! ## Layout (multi-tenant)
//!
//! ```text
//! <config_dir>/profiles/<tenant_id>/
//! ├── data/                         ← SessionScope.root
//! │   ├── users/<session_id>/
//! │   │   └── workspace/            ← SessionScope.workspace (per-session, ephemeral)
//! │   ├── research/                 ← shared_zones[0] (workers MUST NOT default CWD here)
//! │   ├── skills/                   ← shared_zones[1] (cross-session, persistent)
//! │   └── episodes.redb             ← OutOfScope (memory store accessed via API, not as CWD or path)
//! ├── config.json
//! └── ...
//! ```
//!
//! ## Layout (solo)
//!
//! ```text
//! <user_cwd>/                       ← SessionScope.root == SessionScope.workspace
//! ├── .octos/                       ← session state, not a separate scope
//! └── <user files>
//! ```
//!
//! ## Component obligations
//!
//! Every component that needs a CWD or validates a path:
//!
//! 1. Receives a `&SessionScope` from `PipelineHostContext`,
//!    `ToolContext`, or an equivalent host-provided context. It does
//!    NOT compute paths from `data_dir`, `profile_id`, session ids,
//!    or env vars itself.
//! 2. Spawns child processes with `current_dir(scope.workspace())`.
//! 3. Validates every user/LLM-supplied path against
//!    [`SessionScope::classify_lexical_path`] before opening it.
//!    Refuses `PathClassification::OutOfScope`.
//! 4. Reports outputs back to the host as `files_to_send: [...]`
//!    listing absolute paths. The host validates each entry against
//!    the same scope.
//!
//! ## What this module does NOT do
//!
//! - It does not perform any I/O. Callers are responsible for
//!   creating the workspace directory, cleaning it up, etc.
//! - It does not enforce path validation at the OS level. The
//!   classification helpers are a logical guard; sandboxes still
//!   apply for defence in depth.
//! - It does not specify the `files_to_send` envelope format —
//!   that's defined in the plugin protocol; this type just provides
//!   the validator the host uses.
//!
//! ## Versioning
//!
//! [`SESSION_SCOPE_SCHEMA_VERSION`] is incremented on incompatible
//! changes to the [`SessionScope`] shape. The schema is wire-relevant
//! only in diagnostics (debug status endpoints); the type is not part
//! of the JSON-RPC public surface.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Schema version of the [`SessionScope`] shape. Bump on incompatible
/// changes.
pub const SESSION_SCOPE_SCHEMA_VERSION: u32 = 1;

/// Default name of the per-session workspace subdirectory inside
/// `<root>/users/<session_id>/`. Held as a constant so the resolver
/// in `handlers.rs` and any future migration share the literal.
pub const MULTI_TENANT_WORKSPACE_DIR_NAME: &str = "workspace";

/// Default name of the per-tenant `users` directory inside
/// `<profile>/data/`. The on-disk structure is
/// `<root>/users/<session_id>/<MULTI_TENANT_WORKSPACE_DIR_NAME>/`.
pub const MULTI_TENANT_USERS_DIR_NAME: &str = "users";

/// Errors that the [`SessionScope`] constructors and helpers can
/// return. All variants describe invariant violations the caller
/// should treat as configuration bugs, not user input failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionScopeError {
    /// Provided `root` is a relative path. SessionScope requires
    /// absolute paths so callers cannot accidentally reinterpret
    /// the scope against different CWDs.
    RootNotAbsolute(PathBuf),

    /// Provided `workspace` is not inside `root`. The constructor
    /// refuses this combination — a workspace outside its root is
    /// always a contract violation.
    WorkspaceEscapesRoot { root: PathBuf, workspace: PathBuf },

    /// Multi-tenant scope was constructed with an empty tenant id.
    EmptyTenantId,

    /// Session id contains characters the on-disk path layout cannot
    /// accept safely. See [`is_safe_session_id`] for the allowed
    /// alphabet.
    UnsafeSessionId(String),

    /// A `granted_dir` passed to a Solo-mode scope is not absolute.
    /// Granted dirs must be absolute so they can be compared
    /// unambiguously against caller paths.
    GrantedDirNotAbsolute(usize, PathBuf),

    /// A shared zone provided to a multi-tenant scope is not a
    /// strict subdir of `root`. The bare `<root>` is also rejected
    /// — modelling the entire root as "shared" makes the
    /// `OutOfScope` classification unreachable (codex round-1).
    SharedZoneNotStrictSubdir { root: PathBuf, zone: PathBuf },

    /// `with_granted_dir` called on a multi-tenant scope. Grants are
    /// a Solo-mode concept; multi-tenant boundaries are enforced by
    /// the path layout. If you need broader access in a multi-tenant
    /// process, model that as a separate maintenance capability, not
    /// an ordinary session grant.
    GrantNotAllowedInMultiTenant,

    /// Shared zone overlaps with the per-session `users/` subtree.
    /// Per codex round-2: if `<root>/users` (or a descendant) were a
    /// shared zone, classify would treat another session's workspace
    /// files as `InSharedZone`, defeating per-session isolation.
    SharedZoneOverlapsUsersSubtree {
        users_subtree: PathBuf,
        zone: PathBuf,
    },
}

impl std::fmt::Display for SessionScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootNotAbsolute(p) => {
                write!(
                    f,
                    "SessionScope.root must be absolute, got: {}",
                    p.display()
                )
            }
            Self::WorkspaceEscapesRoot { root, workspace } => write!(
                f,
                "SessionScope.workspace ({}) must be inside root ({})",
                workspace.display(),
                root.display()
            ),
            Self::EmptyTenantId => {
                write!(f, "SessionScope.MultiTenant requires a non-empty tenant_id")
            }
            Self::UnsafeSessionId(id) => {
                write!(f, "session_id {id:?} contains unsafe characters")
            }
            Self::GrantedDirNotAbsolute(idx, p) => write!(
                f,
                "Solo.granted_dirs[{idx}] must be absolute, got: {}",
                p.display()
            ),
            Self::SharedZoneNotStrictSubdir { root, zone } => write!(
                f,
                "shared zone ({}) must be a strict subdir of root ({}); the bare root is rejected",
                zone.display(),
                root.display()
            ),
            Self::GrantNotAllowedInMultiTenant => write!(
                f,
                "with_granted_dir is Solo-only; multi-tenant boundaries are enforced by path layout"
            ),
            Self::SharedZoneOverlapsUsersSubtree {
                users_subtree,
                zone,
            } => write!(
                f,
                "shared zone ({}) overlaps the per-session users subtree ({}); cross-session isolation requires zones live outside `users/`",
                zone.display(),
                users_subtree.display()
            ),
        }
    }
}

impl std::error::Error for SessionScopeError {}

/// Classification of a path relative to a [`SessionScope`]. Every
/// path validator across octos must return this shape — there are no
/// custom validation results.
///
/// `Serialize` only: deserialisation would let callers bypass the
/// constructor invariants on [`SessionScope`] (e.g. construct a
/// classification that doesn't match the actual scope). Diagnostics
/// emit these; consumers compare against the live scope's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PathClassification {
    /// Path is inside `scope.workspace`. Writes and reads allowed.
    /// This is the default-allowed zone for plugin outputs, file
    /// tools, shell, etc.
    InWorkspace,
    /// Path is inside one of `scope.shared_zones` (multi-tenant only).
    /// Reads allowed when the caller declares intent (e.g.
    /// `recall(<dir>)`); writes refused — shared data is managed
    /// by maintenance code paths, not session workers.
    InSharedZone { zone: PathBuf },
    /// Path is inside one of `Solo.granted_dirs` (solo only).
    /// Reads and writes allowed; the user explicitly granted access
    /// via a Claude-Code-style permission prompt.
    InGrantedDir { granted_dir: PathBuf },
    /// Path is outside every declared zone (workspace, shared_zones,
    /// granted_dirs). Refuse — this is either a tenant-boundary
    /// escape (multi-tenant) or a path the user has not granted
    /// (solo). The previous `InRootButOutsideZones` variant was
    /// dropped per codex round-1 review: with `shared_data == root`
    /// it was unreachable; with named shared zones, there are no
    /// "almost legitimate" paths to distinguish from full escapes.
    OutOfScope,
}

/// The mode-specific portion of a [`SessionScope`]. Determines the
/// validator's policy: strict tenant isolation vs Claude-Code-style
/// user-managed permissions.
///
/// `Serialize` only — see [`SessionScope`] for the rationale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScopeMode {
    /// Strict per-tenant + per-session isolation. The host process
    /// serves multiple tenants; each tenant gets its own root; each
    /// session inside a tenant gets its own workspace.
    ///
    /// `shared_zones` is the list of declared cross-session zones
    /// inside `<root>` that LLMs may read with explicit intent (e.g.
    /// `<root>/research/`, `<root>/skills/`). Workers MUST NOT
    /// default to any of these as CWD. Each entry MUST be a strict
    /// subdir of `scope.root` (validated at construction; the bare
    /// `<root>` is rejected).
    MultiTenant {
        /// Stable tenant identifier (the `profile_id`). Used in
        /// diagnostics and to disambiguate cross-tenant leaks; the
        /// path layout itself enforces the boundary.
        tenant_id: String,
        /// Stable session identifier within the tenant. Must satisfy
        /// [`is_safe_session_id`] when the scope is constructed.
        session_id: String,
        /// Declared cross-session zones. See type doc.
        shared_zones: Vec<PathBuf>,
    },
    /// Single-user mode: the user's CWD is the scope. Cross-session
    /// continuity is intentional. Permission grants extend the scope
    /// to additional dirs the user explicitly approves.
    Solo {
        /// Additional directories the user has granted access to,
        /// outside `scope.root`. Each entry must be absolute.
        ///
        /// Empty by default. Grants accumulate over the lifetime of
        /// the process (or until revoked); they do not persist across
        /// restarts unless the host serialises them elsewhere.
        granted_dirs: Vec<PathBuf>,
    },
}

/// The single filesystem contract for an octos session.
///
/// Constructed by the host (`octos serve` or `octos chat`) once per
/// session and threaded into every component that needs a CWD or
/// path validation. See module-level docs for the obligations of
/// downstream consumers.
///
/// Fields are private; access goes through accessor methods so the
/// invariants enforced by constructors hold for the lifetime of the
/// value (the type is immutable after construction; mode-specific
/// mutations like adding a granted dir produce a new `SessionScope`).
///
/// `Serialize` only, no `Deserialize`: deserialisation would bypass
/// the constructor invariants (per codex round-1 review — a `Solo`
/// scope could deserialise with multi-tenant-shaped fields, or a
/// relative `root` could land in production code via JSON). For
/// diagnostics emit-only is sufficient; for any wire-input case use
/// a separate `SessionScopeRequest` shape + `TryFrom`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionScope {
    /// Outermost boundary. No path validated against this scope
    /// returns `PathClassification::InWorkspace`, `InSharedZone`,
    /// or `InGrantedDir` unless it falls inside `root` (or, for
    /// solo, inside a granted dir which may be outside `root`).
    root: PathBuf,
    /// Per-session ephemeral workspace. Workers and plugins spawn
    /// with this as their CWD. Empty at session start (for
    /// multi-tenant) or = `root` (for solo).
    workspace: PathBuf,
    /// Mode-specific policy. See [`ScopeMode`]. `shared_zones` is now
    /// folded into `ScopeMode::MultiTenant` — solo has no shared zones
    /// concept and the old `shared_data: Option<PathBuf>` modelled
    /// that with a runtime-checked `None` instead of using the type
    /// system. Per codex round-1: typed mode-specific fields prevent
    /// illegal-state construction.
    mode: ScopeMode,
}

/// Canonical names of shared zones under `<root>` for the dspfac /
/// mofa multi-tenant deploy. Callers should prefer
/// [`SessionScope::multi_tenant_with_default_zones`] which references
/// these; this list exists so the constants are reusable in tests
/// and migration audits.
pub const DEFAULT_MULTI_TENANT_SHARED_ZONE_NAMES: &[&str] = &["research", "skills"];

impl SessionScope {
    /// Construct a multi-tenant scope from the canonical layout, with
    /// custom shared zones.
    ///
    /// - `profile_data_dir` is `<config_dir>/profiles/<tenant_id>/data/`.
    ///   It becomes `root`.
    /// - `<root>/users/<session_id>/workspace/` becomes `workspace`.
    /// - Each entry in `shared_zones` MUST be a strict subdir of
    ///   `root` (the bare `<root>` is rejected per codex round-1).
    ///
    /// Validates that `profile_data_dir` is absolute, that
    /// `session_id` satisfies [`is_safe_session_id`], and that every
    /// `shared_zones` entry is a strict subdir of `root`.
    ///
    /// Does NOT create the workspace directory on disk. Callers
    /// (the WS turn handler or session opener) are responsible for
    /// `std::fs::create_dir_all(scope.workspace())` before spawning
    /// workers.
    pub fn multi_tenant(
        profile_data_dir: PathBuf,
        tenant_id: String,
        session_id: String,
        shared_zones: Vec<PathBuf>,
    ) -> Result<Self, SessionScopeError> {
        if !profile_data_dir.is_absolute() {
            return Err(SessionScopeError::RootNotAbsolute(profile_data_dir));
        }
        if tenant_id.is_empty() {
            return Err(SessionScopeError::EmptyTenantId);
        }
        if !is_safe_session_id(&session_id) {
            return Err(SessionScopeError::UnsafeSessionId(session_id));
        }
        let users_subtree = profile_data_dir.join(MULTI_TENANT_USERS_DIR_NAME);
        for zone in &shared_zones {
            // Must be a strict subdir of root.
            if zone == &profile_data_dir || !zone.starts_with(&profile_data_dir) {
                return Err(SessionScopeError::SharedZoneNotStrictSubdir {
                    root: profile_data_dir.clone(),
                    zone: zone.clone(),
                });
            }
            // Per codex round-2 P2: must NOT overlap with the per-
            // session users subtree. If `<root>/users` (or a child)
            // were a shared zone, cross-session workspace files
            // would be classified as `InSharedZone`, defeating
            // session isolation. Refuse `<root>/users` and any
            // descendant.
            if zone == &users_subtree || zone.starts_with(&users_subtree) {
                return Err(SessionScopeError::SharedZoneOverlapsUsersSubtree {
                    users_subtree: users_subtree.clone(),
                    zone: zone.clone(),
                });
            }
        }
        let workspace = profile_data_dir
            .join(MULTI_TENANT_USERS_DIR_NAME)
            .join(&session_id)
            .join(MULTI_TENANT_WORKSPACE_DIR_NAME);
        Ok(Self {
            workspace,
            root: profile_data_dir,
            mode: ScopeMode::MultiTenant {
                tenant_id,
                session_id,
                shared_zones,
            },
        })
    }

    /// Convenience constructor that derives shared zones from
    /// [`DEFAULT_MULTI_TENANT_SHARED_ZONE_NAMES`]. Use this for the
    /// stock dspfac / mofa layout; use [`Self::multi_tenant`] when
    /// the tenant needs a different set.
    pub fn multi_tenant_with_default_zones(
        profile_data_dir: PathBuf,
        tenant_id: String,
        session_id: String,
    ) -> Result<Self, SessionScopeError> {
        let shared_zones = DEFAULT_MULTI_TENANT_SHARED_ZONE_NAMES
            .iter()
            .map(|name| profile_data_dir.join(name))
            .collect();
        Self::multi_tenant(profile_data_dir, tenant_id, session_id, shared_zones)
    }

    /// Construct a solo scope from the user's CWD. Workspace == root
    /// (one CWD per process); no shared zones (cross-session
    /// continuity is the user's project files in their CWD, not a
    /// separate zone).
    ///
    /// Validates that `cwd` is absolute and that each entry in
    /// `granted_dirs` is absolute.
    pub fn solo(cwd: PathBuf, granted_dirs: Vec<PathBuf>) -> Result<Self, SessionScopeError> {
        if !cwd.is_absolute() {
            return Err(SessionScopeError::RootNotAbsolute(cwd));
        }
        for (idx, dir) in granted_dirs.iter().enumerate() {
            if !dir.is_absolute() {
                return Err(SessionScopeError::GrantedDirNotAbsolute(idx, dir.clone()));
            }
        }
        Ok(Self {
            workspace: cwd.clone(),
            root: cwd,
            mode: ScopeMode::Solo { granted_dirs },
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Return the declared cross-session shared zones (multi-tenant
    /// only). Empty slice for solo. Callers should prefer this over
    /// reaching into [`ScopeMode::MultiTenant::shared_zones`]
    /// directly so the API doesn't churn if the field moves.
    pub fn shared_zones(&self) -> &[PathBuf] {
        match &self.mode {
            ScopeMode::MultiTenant { shared_zones, .. } => shared_zones,
            ScopeMode::Solo { .. } => &[],
        }
    }

    pub fn mode(&self) -> &ScopeMode {
        &self.mode
    }

    /// Return a new `SessionScope` with `dir` added to `granted_dirs`.
    /// Solo-mode only. Calling on multi-tenant returns
    /// [`SessionScopeError::GrantNotAllowedInMultiTenant`] per codex
    /// round-1: silent no-op invites callers to assume the grant
    /// applied when it didn't.
    pub fn with_granted_dir(mut self, dir: PathBuf) -> Result<Self, SessionScopeError> {
        if !dir.is_absolute() {
            return Err(SessionScopeError::GrantedDirNotAbsolute(0, dir));
        }
        match &mut self.mode {
            ScopeMode::Solo { granted_dirs } => {
                if !granted_dirs.iter().any(|d| d == &dir) {
                    granted_dirs.push(dir);
                }
                Ok(self)
            }
            ScopeMode::MultiTenant { .. } => Err(SessionScopeError::GrantNotAllowedInMultiTenant),
        }
    }

    /// Classify `path` against this scope using pure-lexical rules.
    /// The single validator that every component must use; bespoke
    /// equivalents in the codebase should migrate to this and be
    /// deleted.
    ///
    /// `lexical` = collapses `.` components, refuses `..`, does NOT
    /// resolve symlinks or canonicalise. Callers needing symlink-safe
    /// checks must additionally use `symlink_metadata().is_file()`
    /// per the #1189 round-2 codex finding (or a future
    /// `classify_resolved_path` variant when it lands).
    ///
    /// Renamed from `classify_path` per codex round-1: the explicit
    /// `_lexical_` prefix prevents callers from over-trusting this
    /// as the filesystem boundary. Sandboxes still apply for defence
    /// in depth.
    pub fn classify_lexical_path(&self, path: &Path) -> PathClassification {
        // Lexical normalisation: collapse `.` components and refuse
        // any `..` we encounter. Real `..` handling belongs in the
        // caller's input validator (see #1186); by the time a path
        // reaches `classify_lexical_path`, callers are expected to
        // have already refused traversal sequences.
        let normalised = match lexical_normalise(path) {
            Some(p) => p,
            None => return PathClassification::OutOfScope,
        };
        // Workspace is most specific — check first.
        if normalised.starts_with(&self.workspace) {
            return PathClassification::InWorkspace;
        }
        match &self.mode {
            ScopeMode::Solo { granted_dirs } => {
                for granted in granted_dirs {
                    if normalised.starts_with(granted) {
                        return PathClassification::InGrantedDir {
                            granted_dir: granted.clone(),
                        };
                    }
                }
            }
            ScopeMode::MultiTenant { shared_zones, .. } => {
                for zone in shared_zones {
                    if normalised.starts_with(zone) {
                        return PathClassification::InSharedZone { zone: zone.clone() };
                    }
                }
            }
        }
        PathClassification::OutOfScope
    }
}

/// Allowed alphabet for session ids that participate in the on-disk
/// path layout (`<root>/users/<session_id>/workspace/`). Mirrors
/// `is_bare_path_safe_session_id` in `handlers.rs` (added by codex
/// P1 of PR #1069) — this is its canonical home; the handler-side
/// helper should migrate to call this.
///
/// Allowed: alphanumeric, `-`, `_`, `#` (the SPA emits `#` between
/// a base session id and its topic suffix). Refuses `.`, `..`, `/`,
/// `\`, NUL, and any non-ASCII byte.
pub fn is_safe_session_id(session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    if session_id == "." || session_id == ".." {
        return false;
    }
    session_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'#')
}

/// Lexically normalise a path: collapse `.` components, refuse any
/// `..` component. Returns `None` if a `..` is present (caller
/// should treat as `OutOfScope`).
///
/// Intentionally pure-lexical — no symlink resolution, no
/// filesystem queries.
fn lexical_normalise(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(part) => out.push(part),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(s: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{s}"))
        } else {
            PathBuf::from(s)
        }
    }

    fn mt_default(data: &Path, session: &str) -> SessionScope {
        SessionScope::multi_tenant_with_default_zones(
            data.to_path_buf(),
            "dspfac".into(),
            session.into(),
        )
        .unwrap()
    }

    #[test]
    fn multi_tenant_layout_matches_handlers_rs_today() {
        let data = abs("/octos/profiles/dspfac/data");
        let scope = mt_default(&data, "web-1779574360679-o8x9kv");
        assert_eq!(scope.root(), data);
        assert_eq!(
            scope.workspace(),
            data.join("users/web-1779574360679-o8x9kv/workspace")
        );
        // shared_zones is the canonical {research, skills} pair
        assert_eq!(
            scope.shared_zones(),
            &[data.join("research"), data.join("skills")]
        );
    }

    #[test]
    fn solo_collapses_workspace_to_cwd_and_no_shared_zones() {
        let cwd = abs("/home/yc/my-project");
        let scope = SessionScope::solo(cwd.clone(), vec![]).unwrap();
        assert_eq!(scope.root(), cwd);
        assert_eq!(scope.workspace(), cwd);
        assert!(scope.shared_zones().is_empty());
    }

    #[test]
    fn refuses_relative_root() {
        let err = SessionScope::multi_tenant_with_default_zones(
            PathBuf::from("relative/data"),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap_err();
        assert!(matches!(err, SessionScopeError::RootNotAbsolute(_)));
    }

    #[test]
    fn refuses_unsafe_session_id() {
        for bad in ["../escape", "/abs", "foo/bar", "..", ".", "with space", ""] {
            let err = SessionScope::multi_tenant_with_default_zones(
                abs("/data"),
                "dspfac".into(),
                bad.into(),
            )
            .unwrap_err();
            assert!(
                matches!(err, SessionScopeError::UnsafeSessionId(_)),
                "expected UnsafeSessionId for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn refuses_bare_root_as_shared_zone() {
        let data = abs("/data");
        let err = SessionScope::multi_tenant(
            data.clone(),
            "dspfac".into(),
            "web-1".into(),
            vec![data.clone()],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SessionScopeError::SharedZoneNotStrictSubdir { .. }
        ));
    }

    #[test]
    fn refuses_shared_zone_outside_root() {
        let err = SessionScope::multi_tenant(
            abs("/data"),
            "dspfac".into(),
            "web-1".into(),
            vec![abs("/elsewhere/research")],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SessionScopeError::SharedZoneNotStrictSubdir { .. }
        ));
    }

    #[test]
    fn refuses_shared_zone_overlapping_users_subtree() {
        // Codex round-2 P2: if <root>/users (or a child) is a shared
        // zone, another session's workspace files classify as
        // InSharedZone, defeating isolation. Reject at construction.
        let data = abs("/octos/profiles/dspfac/data");
        for bad in [data.join("users"), data.join("users/web-7/workspace")] {
            let err = SessionScope::multi_tenant(
                data.clone(),
                "dspfac".into(),
                "web-1".into(),
                vec![bad.clone()],
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    SessionScopeError::SharedZoneOverlapsUsersSubtree { .. }
                ),
                "expected SharedZoneOverlapsUsersSubtree for zone {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn another_sessions_workspace_classifies_as_out_of_scope() {
        // End-to-end check on the P2 fix: even if a malicious
        // constructor call had slipped past, classify_lexical_path
        // for another session's workspace under our scope must NOT
        // return InSharedZone or InWorkspace.
        let data = abs("/octos/profiles/dspfac/data");
        let scope = mt_default(&data, "web-1");
        let other_session_workspace = data.join("users/web-2/workspace/secret.md");
        assert_eq!(
            scope.classify_lexical_path(&other_session_workspace),
            PathClassification::OutOfScope
        );
    }

    #[test]
    fn accepts_topic_suffix_in_session_id() {
        let scope = mt_default(&abs("/data"), "web-123#slides");
        assert!(
            scope
                .workspace()
                .ends_with("users/web-123#slides/workspace")
        );
    }

    #[test]
    fn classify_path_in_workspace() {
        let scope = mt_default(&abs("/octos/profiles/dspfac/data"), "web-1");
        let path = abs("/octos/profiles/dspfac/data/users/web-1/workspace/script.md");
        assert_eq!(
            scope.classify_lexical_path(&path),
            PathClassification::InWorkspace
        );
    }

    #[test]
    fn classify_path_in_shared_zone_returns_zone_path() {
        let data = abs("/octos/profiles/dspfac/data");
        let scope = mt_default(&data, "web-1");
        let path = data.join("research/jwst/notes.md");
        assert_eq!(
            scope.classify_lexical_path(&path),
            PathClassification::InSharedZone {
                zone: data.join("research")
            }
        );
    }

    #[test]
    fn classify_path_out_of_scope_for_path_inside_root_but_outside_zones() {
        // Per codex round-1: with named shared zones, paths under
        // `<root>` but outside the declared zones are OutOfScope
        // (was the dropped `InRootButOutsideZones` variant). E.g.
        // `<root>/episodes.redb` is system internals, not a managed
        // zone for LLM access.
        let data = abs("/octos/profiles/dspfac/data");
        let scope = mt_default(&data, "web-1");
        let path = data.join("episodes.redb");
        assert_eq!(
            scope.classify_lexical_path(&path),
            PathClassification::OutOfScope
        );
    }

    #[test]
    fn classify_path_out_of_scope_for_other_tenant() {
        let scope = mt_default(&abs("/octos/profiles/dspfac/data"), "web-1");
        let path = abs("/octos/profiles/acme/data/research/secret.md");
        assert_eq!(
            scope.classify_lexical_path(&path),
            PathClassification::OutOfScope
        );
    }

    #[test]
    fn classify_path_refuses_parent_dir_components() {
        let scope = mt_default(&abs("/octos/profiles/dspfac/data"), "web-1");
        let path = abs("/octos/profiles/dspfac/data/users/web-1/workspace/../../../../etc/passwd");
        assert_eq!(
            scope.classify_lexical_path(&path),
            PathClassification::OutOfScope
        );
    }

    #[test]
    fn solo_classify_path_in_workspace_for_anything_under_cwd() {
        let cwd = abs("/home/yc/my-project");
        let scope = SessionScope::solo(cwd.clone(), vec![]).unwrap();
        assert_eq!(
            scope.classify_lexical_path(&cwd.join("src/main.rs")),
            PathClassification::InWorkspace
        );
    }

    #[test]
    fn solo_classify_path_in_granted_dir() {
        let cwd = abs("/home/yc/my-project");
        let grant = abs("/tmp/scratch");
        let scope = SessionScope::solo(cwd, vec![grant.clone()]).unwrap();
        assert_eq!(
            scope.classify_lexical_path(&grant.join("foo.txt")),
            PathClassification::InGrantedDir {
                granted_dir: grant.clone()
            }
        );
    }

    #[test]
    fn solo_classify_path_out_of_scope_when_no_grant() {
        let cwd = abs("/home/yc/my-project");
        let scope = SessionScope::solo(cwd, vec![]).unwrap();
        assert_eq!(
            scope.classify_lexical_path(&abs("/etc/passwd")),
            PathClassification::OutOfScope
        );
    }

    #[test]
    fn with_granted_dir_is_idempotent_in_solo() {
        let cwd = abs("/home/yc/my-project");
        let grant = abs("/tmp/scratch");
        let scope = SessionScope::solo(cwd, vec![]).unwrap();
        let scope = scope.with_granted_dir(grant.clone()).unwrap();
        let scope = scope.with_granted_dir(grant.clone()).unwrap();
        if let ScopeMode::Solo { granted_dirs } = scope.mode() {
            assert_eq!(granted_dirs.len(), 1);
            assert_eq!(&granted_dirs[0], &grant);
        } else {
            panic!("expected Solo");
        }
    }

    #[test]
    fn with_granted_dir_errors_in_multi_tenant() {
        // Per codex round-1: silent no-op invites callers to assume
        // the grant applied when it didn't. MultiTenant has no grant
        // concept; return Err instead.
        let scope = mt_default(&abs("/octos/profiles/dspfac/data"), "web-1");
        let err = scope.with_granted_dir(abs("/tmp/scratch")).unwrap_err();
        assert!(matches!(
            err,
            SessionScopeError::GrantNotAllowedInMultiTenant
        ));
    }
}
