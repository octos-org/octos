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
//! │   ├── skills/                   ← SessionScope.shared_data (cross-session, persistent)
//! │   ├── research/                 ← SessionScope.shared_data (workers MUST NOT default CWD here)
//! │   └── episodes.redb             ← memory store (accessed via API, not as CWD)
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
//!    [`SessionScope::classify_path`] before opening it. Refuses
//!    `PathClassification::OutOfScope`.
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

use serde::{Deserialize, Serialize};

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
        }
    }
}

impl std::error::Error for SessionScopeError {}

/// Classification of a path relative to a [`SessionScope`]. Every
/// path validator across octos must return this shape — there are no
/// custom validation results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PathClassification {
    /// Path is inside `scope.workspace`. Writes and reads allowed.
    /// This is the default-allowed zone for plugin outputs, file
    /// tools, shell, etc.
    InWorkspace,
    /// Path is inside `scope.shared_data` (multi-tenant only).
    /// Reads allowed when the caller declares intent (e.g.
    /// `recall(<dir>)`); writes refused — shared data is managed
    /// by maintenance code paths, not session workers.
    InSharedData,
    /// Path is inside one of `Solo.granted_dirs` (solo only).
    /// Reads and writes allowed; the user explicitly granted access
    /// via a Claude-Code-style permission prompt.
    InGrantedDir { granted_dir: PathBuf },
    /// Path is inside `scope.root` but does not match a more specific
    /// zone above. Caller MAY refuse depending on policy; most
    /// callers should treat this as deny.
    InRootButOutsideZones,
    /// Path is outside `scope.root`. Refuse unconditionally — this is
    /// either a tenant-boundary escape (multi-tenant) or a path the
    /// user has not granted (solo).
    OutOfScope,
}

/// The mode-specific portion of a [`SessionScope`]. Determines the
/// validator's policy: strict tenant isolation vs Claude-Code-style
/// user-managed permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScopeMode {
    /// Strict per-tenant + per-session isolation. The host process
    /// serves multiple tenants; each tenant gets its own root; each
    /// session inside a tenant gets its own workspace.
    MultiTenant {
        /// Stable tenant identifier (the `profile_id`). Used in
        /// diagnostics and to disambiguate cross-tenant leaks; the
        /// path layout itself enforces the boundary.
        tenant_id: String,
        /// Stable session identifier within the tenant. Must satisfy
        /// [`is_safe_session_id`] when the scope is constructed.
        session_id: String,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionScope {
    /// Outermost boundary. No path validated against this scope
    /// returns `PathClassification::InWorkspace` or `InSharedData`
    /// unless it is inside `root`.
    root: PathBuf,
    /// Per-session ephemeral workspace. Workers and plugins spawn
    /// with this as their CWD. Empty at session start (for
    /// multi-tenant) or = `root` (for solo).
    workspace: PathBuf,
    /// Cross-session persistent zone within `root`. Workers MUST
    /// NOT default to this as a CWD. Reads allowed via declared-
    /// intent APIs (the `recall` tool, an explicit `read_file` on a
    /// path the LLM names). `None` in solo mode — solo has no
    /// cross-session concept distinct from the workspace itself.
    shared_data: Option<PathBuf>,
    /// Mode-specific policy. See [`ScopeMode`].
    mode: ScopeMode,
}

impl SessionScope {
    /// Construct a multi-tenant scope from the canonical layout:
    ///
    /// - `profile_data_dir` is `<config_dir>/profiles/<tenant_id>/data/`.
    ///   It becomes `root`.
    /// - `<root>/users/<session_id>/workspace/` becomes `workspace`.
    /// - `root` itself becomes `shared_data` (subdirs like `skills/`
    ///   and `research/` live there).
    ///
    /// Validates that `profile_data_dir` is absolute and that
    /// `session_id` satisfies [`is_safe_session_id`].
    ///
    /// Does NOT create the workspace directory on disk. Callers
    /// (the WS turn handler or session opener) are responsible for
    /// `std::fs::create_dir_all(scope.workspace())` before spawning
    /// workers.
    pub fn multi_tenant(
        profile_data_dir: PathBuf,
        tenant_id: String,
        session_id: String,
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
        let workspace = profile_data_dir
            .join(MULTI_TENANT_USERS_DIR_NAME)
            .join(&session_id)
            .join(MULTI_TENANT_WORKSPACE_DIR_NAME);
        let root = profile_data_dir;
        Ok(Self {
            shared_data: Some(root.clone()),
            workspace,
            root,
            mode: ScopeMode::MultiTenant {
                tenant_id,
                session_id,
            },
        })
    }

    /// Construct a solo scope from the user's CWD. Workspace == root
    /// (one CWD per process); no shared_data (cross-session continuity
    /// is the user's project files in their CWD, not a separate zone).
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
            shared_data: None,
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

    pub fn shared_data(&self) -> Option<&Path> {
        self.shared_data.as_deref()
    }

    pub fn mode(&self) -> &ScopeMode {
        &self.mode
    }

    /// Return a new `SessionScope` with `dir` added to `granted_dirs`.
    /// Solo-mode only; returns the scope unchanged in MultiTenant mode
    /// (MultiTenant has no grant concept — the path layout is the
    /// only boundary).
    pub fn with_granted_dir(mut self, dir: PathBuf) -> Result<Self, SessionScopeError> {
        if !dir.is_absolute() {
            return Err(SessionScopeError::GrantedDirNotAbsolute(0, dir));
        }
        if let ScopeMode::Solo { granted_dirs } = &mut self.mode {
            if !granted_dirs.iter().any(|d| d == &dir) {
                granted_dirs.push(dir);
            }
        }
        Ok(self)
    }

    /// Classify `path` against this scope. The single validator that
    /// every component must use; bespoke equivalents in the codebase
    /// should migrate to this and be deleted.
    ///
    /// Path is normalised lexically only (no symlink resolution, no
    /// canonicalisation that requires the path to exist). Callers
    /// that need symlink-safe checks should additionally use
    /// `symlink_metadata().is_file()` per the #1189 round-2 codex
    /// finding; this validator is a logical guard, not an OS-level
    /// one.
    ///
    /// NOTE: lexical-only classification means a symlink pointing
    /// outside the scope returns `InWorkspace` if the symlink itself
    /// lives in the workspace. Sandboxes provide the OS-level guard;
    /// this method is intentionally cheap and stateless.
    pub fn classify_path(&self, path: &Path) -> PathClassification {
        // Lexical normalisation: collapse `.` components and refuse
        // any `..` we encounter. Real `..` handling belongs in the
        // caller's input validator (see #1186); by the time a path
        // reaches `classify_path`, callers are expected to have
        // already refused traversal sequences.
        let normalised = match lexical_normalise(path) {
            Some(p) => p,
            None => return PathClassification::OutOfScope,
        };
        // Check more specific zones first; fall back to root, then
        // out-of-scope.
        if normalised.starts_with(&self.workspace) {
            return PathClassification::InWorkspace;
        }
        if let ScopeMode::Solo { granted_dirs } = &self.mode {
            for granted in granted_dirs {
                if normalised.starts_with(granted) {
                    return PathClassification::InGrantedDir {
                        granted_dir: granted.clone(),
                    };
                }
            }
        }
        if let Some(shared) = &self.shared_data
            && normalised.starts_with(shared)
            && !normalised.starts_with(&self.workspace)
        {
            return PathClassification::InSharedData;
        }
        if normalised.starts_with(&self.root) {
            return PathClassification::InRootButOutsideZones;
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

    #[test]
    fn multi_tenant_layout_matches_handlers_rs_today() {
        let data = abs("/octos/profiles/dspfac/data");
        let scope = SessionScope::multi_tenant(
            data.clone(),
            "dspfac".into(),
            "web-1779574360679-o8x9kv".into(),
        )
        .unwrap();
        assert_eq!(scope.root(), data);
        assert_eq!(
            scope.workspace(),
            data.join("users/web-1779574360679-o8x9kv/workspace")
        );
        assert_eq!(scope.shared_data(), Some(data.as_path()));
    }

    #[test]
    fn solo_collapses_workspace_to_cwd_and_no_shared_data() {
        let cwd = abs("/home/yc/my-project");
        let scope = SessionScope::solo(cwd.clone(), vec![]).unwrap();
        assert_eq!(scope.root(), cwd);
        assert_eq!(scope.workspace(), cwd);
        assert_eq!(scope.shared_data(), None);
    }

    #[test]
    fn refuses_relative_root() {
        let err = SessionScope::multi_tenant(
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
            let err =
                SessionScope::multi_tenant(abs("/data"), "dspfac".into(), bad.into()).unwrap_err();
            assert!(
                matches!(err, SessionScopeError::UnsafeSessionId(_)),
                "expected UnsafeSessionId for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn accepts_topic_suffix_in_session_id() {
        let scope =
            SessionScope::multi_tenant(abs("/data"), "dspfac".into(), "web-123#slides".into())
                .unwrap();
        assert!(
            scope
                .workspace()
                .ends_with("users/web-123#slides/workspace")
        );
    }

    #[test]
    fn classify_path_in_workspace() {
        let scope = SessionScope::multi_tenant(
            abs("/octos/profiles/dspfac/data"),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let path = abs("/octos/profiles/dspfac/data/users/web-1/workspace/script.md");
        assert_eq!(scope.classify_path(&path), PathClassification::InWorkspace);
    }

    #[test]
    fn classify_path_in_shared_data() {
        let scope = SessionScope::multi_tenant(
            abs("/octos/profiles/dspfac/data"),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let path = abs("/octos/profiles/dspfac/data/research/jwst/notes.md");
        assert_eq!(scope.classify_path(&path), PathClassification::InSharedData);
    }

    #[test]
    fn classify_path_out_of_scope_for_other_tenant() {
        let scope = SessionScope::multi_tenant(
            abs("/octos/profiles/dspfac/data"),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let path = abs("/octos/profiles/acme/data/research/secret.md");
        assert_eq!(scope.classify_path(&path), PathClassification::OutOfScope);
    }

    #[test]
    fn classify_path_refuses_parent_dir_components() {
        let scope = SessionScope::multi_tenant(
            abs("/octos/profiles/dspfac/data"),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let path = abs("/octos/profiles/dspfac/data/users/web-1/workspace/../../../../etc/passwd");
        assert_eq!(scope.classify_path(&path), PathClassification::OutOfScope);
    }

    #[test]
    fn solo_classify_path_in_workspace_for_anything_under_cwd() {
        let cwd = abs("/home/yc/my-project");
        let scope = SessionScope::solo(cwd.clone(), vec![]).unwrap();
        assert_eq!(
            scope.classify_path(&cwd.join("src/main.rs")),
            PathClassification::InWorkspace
        );
    }

    #[test]
    fn solo_classify_path_in_granted_dir() {
        let cwd = abs("/home/yc/my-project");
        let grant = abs("/tmp/scratch");
        let scope = SessionScope::solo(cwd, vec![grant.clone()]).unwrap();
        assert_eq!(
            scope.classify_path(&grant.join("foo.txt")),
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
            scope.classify_path(&abs("/etc/passwd")),
            PathClassification::OutOfScope
        );
    }

    #[test]
    fn with_granted_dir_is_idempotent() {
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
    fn with_granted_dir_is_noop_in_multi_tenant() {
        let scope = SessionScope::multi_tenant(
            abs("/octos/profiles/dspfac/data"),
            "dspfac".into(),
            "web-1".into(),
        )
        .unwrap();
        let scope = scope.with_granted_dir(abs("/tmp/scratch")).unwrap();
        // mode unchanged; no granted_dirs concept in MultiTenant
        assert!(matches!(scope.mode(), ScopeMode::MultiTenant { .. }));
    }
}
