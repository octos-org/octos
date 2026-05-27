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
///
/// Version history:
/// - v1: workspace + shared_zones (MultiTenant) / workspace +
///   granted_dirs (Solo).
/// - v2 (PR-A SKILL.md injection rethink): adds `skill_read_zones`,
///   a read-only allowlist that lets file tools reach plugin
///   `skill_dir` trees. Writes remain workspace-only. Additive — old
///   callers that pass an empty list keep v1 behaviour.
pub const SESSION_SCOPE_SCHEMA_VERSION: u32 = 2;

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

    /// A `skill_read_zone` is not absolute. Skill directories must be
    /// absolute so they can be compared unambiguously against caller
    /// paths (same invariant as `granted_dirs`).
    SkillReadZoneNotAbsolute(usize, PathBuf),
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
            Self::SkillReadZoneNotAbsolute(idx, p) => write!(
                f,
                "skill_read_zones[{idx}] must be absolute, got: {}",
                p.display()
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
    /// Path is inside one of `scope.skill_read_zones` (any mode).
    /// **Reads allowed; writes refused.** Used by the auto-injected
    /// plugin SKILL.md guidance so the agent can `read_file` against
    /// references the SKILL.md mentions (helper scripts, example
    /// data, etc.) without needing per-skill workspace copies.
    /// Workspace and granted_dirs still take precedence — if a path
    /// happens to be inside both, the higher-trust zone wins.
    InSkillDir { skill_dir: PathBuf },
    /// Path is outside every declared zone (workspace, shared_zones,
    /// granted_dirs, skill_read_zones). Refuse — this is either a
    /// tenant-boundary escape (multi-tenant) or a path the user has
    /// not granted (solo). The previous `InRootButOutsideZones`
    /// variant was dropped per codex round-1 review: with
    /// `shared_data == root` it was unreachable; with named shared
    /// zones, there are no "almost legitimate" paths to distinguish
    /// from full escapes.
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
    /// Read-only allowlist of plugin `skill_dir` trees the agent may
    /// reach with read-side file tools (`read_file` today; `glob` /
    /// `grep` / `list_dir` in PR-B). **Reads allowed; writes
    /// refused.** Cross-mode (both solo and multi-tenant carry it)
    /// because the SKILL.md auto-inject in
    /// `octos-agent/src/plugins/extras.rs` happens regardless of
    /// scope mode — the agent needs the same read access in both.
    ///
    /// Empty by default. Callers opt in by passing a non-empty vec
    /// to [`Self::multi_tenant`] / [`Self::solo`] or layering on
    /// with [`Self::with_skill_read_zones`]. Each entry must be
    /// absolute (canonicalisation is the caller's responsibility —
    /// the tools-side `resolve_for_scope` canonicalizes both sides
    /// before comparing, matching how `InSharedZone` is handled).
    ///
    /// Classification order: workspace, then granted_dirs, then
    /// `skill_read_zones`, then shared_zones, then OutOfScope. Per
    /// the PR-A spec, workspace and granted_dirs always win as the
    /// higher-priority classifications even when a skill_dir happens
    /// to live inside them.
    skill_read_zones: Vec<PathBuf>,
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
            skill_read_zones: Vec::new(),
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
            skill_read_zones: Vec::new(),
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

    /// Return the read-only plugin skill directories the agent may
    /// reach. See the `skill_read_zones` field doc on [`SessionScope`].
    pub fn skill_read_zones(&self) -> &[PathBuf] {
        &self.skill_read_zones
    }

    /// Return a new `SessionScope` with `skill_read_zones` replacing
    /// any previously-set list.
    ///
    /// Each entry MUST be absolute. The caller is responsible for
    /// canonicalising paths (the tools-side `resolve_for_scope`
    /// canonicalizes both sides before comparing, matching the
    /// existing `InSharedZone` containment guard).
    ///
    /// Cross-mode: both solo and multi-tenant accept skill read zones
    /// because the auto-injected SKILL.md guidance fires regardless
    /// of scope mode. The classifier consults this list AFTER
    /// workspace and granted_dirs so the higher-trust zones still win
    /// when paths overlap (e.g. a skill_dir nested inside the
    /// workspace would classify as `InWorkspace`, not `InSkillDir`).
    pub fn with_skill_read_zones(mut self, dirs: Vec<PathBuf>) -> Result<Self, SessionScopeError> {
        for (idx, dir) in dirs.iter().enumerate() {
            if !dir.is_absolute() {
                return Err(SessionScopeError::SkillReadZoneNotAbsolute(
                    idx,
                    dir.clone(),
                ));
            }
        }
        self.skill_read_zones = dirs;
        Ok(self)
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
        // Per-mode high-trust zones come next. For solo this is
        // `granted_dirs` (user-approved); for multi-tenant we
        // intentionally defer `shared_zones` until AFTER
        // `skill_read_zones` per the PR-A classification order
        // (workspace > granted_dirs > skill_read_zones > shared_zones).
        if let ScopeMode::Solo { granted_dirs } = &self.mode {
            for granted in granted_dirs {
                if normalised.starts_with(granted) {
                    return PathClassification::InGrantedDir {
                        granted_dir: granted.clone(),
                    };
                }
            }
        }
        // Read-only plugin skill directories. Consulted AFTER
        // workspace and granted_dirs so the higher-priority zones
        // still classify a path inside both (degenerate case where a
        // skill_dir is nested inside the workspace) as `InWorkspace`,
        // not `InSkillDir`. Consulted BEFORE shared_zones because
        // skill dirs are plugin-installed read material, not the
        // declared cross-session "research/skills" zones; treating
        // them the same would let plugin paths inherit
        // `InSharedZone` semantics by accident.
        for skill_dir in &self.skill_read_zones {
            if normalised.starts_with(skill_dir) {
                return PathClassification::InSkillDir {
                    skill_dir: skill_dir.clone(),
                };
            }
        }
        if let ScopeMode::MultiTenant { shared_zones, .. } = &self.mode {
            for zone in shared_zones {
                if normalised.starts_with(zone) {
                    return PathClassification::InSharedZone { zone: zone.clone() };
                }
            }
        }
        PathClassification::OutOfScope
    }

    /// Classify `path` against this scope after canonicalising both
    /// sides. Closes the ancestor-symlink hole in
    /// [`Self::classify_lexical_path`]: a symlink anywhere on the
    /// `path` chain (or inside one of the scope roots) is resolved
    /// before the prefix comparison, so a skill_dir containing
    /// `link -> /outside` cannot smuggle `<skill_dir>/link/secret` as
    /// `InSkillDir`.
    ///
    /// Input MUST be lexically normalised (no `..` components). Callers
    /// that don't have a pre-normalised path should call
    /// [`Self::classify_lexical_path`] first (which rejects `..`) and
    /// only fall through to this method when they need the canonical
    /// containment guarantee.
    ///
    /// Same return shape as [`Self::classify_lexical_path`]. The reported
    /// `skill_dir` / `zone` / `granted_dir` is the un-canonicalised form
    /// (matches what callers configured) so log messages and tests stay
    /// readable; the comparison itself uses canonicalised roots.
    ///
    /// Filesystem access: canonicalises `path` and every zone root via
    /// [`canonicalize_lossy`] (walks ancestors when the leaf is missing,
    /// e.g. for writes that target new files). When a path or root
    /// can't be canonicalised at all, the lossy fallback is the input
    /// itself, which keeps `OutOfScope` as the default-deny answer for
    /// fully-virtual paths.
    pub fn classify_canonical_path(&self, path: &Path) -> PathClassification {
        let canon = canonicalize_lossy(path);
        let canon_ws = canonical_root_lossy(&self.workspace);
        if canon.starts_with(&canon_ws) {
            return PathClassification::InWorkspace;
        }
        if let ScopeMode::Solo { granted_dirs } = &self.mode {
            for granted in granted_dirs {
                let canon_grant = canonical_root_lossy(granted);
                if canon.starts_with(&canon_grant) {
                    return PathClassification::InGrantedDir {
                        granted_dir: granted.clone(),
                    };
                }
            }
        }
        for skill_dir in &self.skill_read_zones {
            let canon_skill = canonical_root_lossy(skill_dir);
            if canon.starts_with(&canon_skill) {
                return PathClassification::InSkillDir {
                    skill_dir: skill_dir.clone(),
                };
            }
        }
        if let ScopeMode::MultiTenant { shared_zones, .. } = &self.mode {
            for zone in shared_zones {
                let canon_zone = canonical_root_lossy(zone);
                if canon.starts_with(&canon_zone) {
                    return PathClassification::InSharedZone { zone: zone.clone() };
                }
            }
        }
        PathClassification::OutOfScope
    }
}

/// Canonicalise a path, walking ancestors when the leaf doesn't exist
/// yet (writes targeting new files inside an existing directory).
/// Mirrors `octos_agent::tools::canonicalize_lossy` (and
/// `octos_bus::file_handle::canonicalize_lossy`). Re-exported here so
/// `SessionScope::classify_canonical_path` and the canonicalize-then-skip
/// helper in the CLI can share one implementation.
///
/// The input must already be lexically normalised (no `..`) so the
/// re-attached suffix names a real would-be on-disk location.
pub fn canonicalize_lossy(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    let mut existing: &Path = path;
    let mut suffix = PathBuf::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            let mut next_suffix = PathBuf::from(name);
            next_suffix.push(&suffix);
            suffix = next_suffix;
        }
        existing = parent;
        if let Ok(canon) = std::fs::canonicalize(existing) {
            return canon.join(suffix);
        }
        if existing.as_os_str().is_empty() {
            break;
        }
    }
    path.to_path_buf()
}

/// Canonicalise a zone root, falling back to the input when the root
/// doesn't exist (rare for `scope.workspace()` but possible in tests
/// that pass a not-yet-created directory).
pub fn canonical_root_lossy(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// Canonicalise a list of skill plugin directories, dropping (with a
/// warning log) any entry whose canonicalisation fails. Fail-closed:
/// when the path can't be canonicalised — typically because it does
/// not exist on disk yet — we drop it rather than keeping the raw form,
/// because a raw path is later vulnerable to symlink replacement
/// (`/tmp/missing -> /etc`) that the canonicalize-on-classify guard
/// would then legitimise.
///
/// Returns the canonicalised list in input order, minus skipped
/// entries. A missing dir has no readable SKILL content yet, so
/// dropping it is the safe fallback; the agent loses a read zone it
/// can't reach anyway.
pub fn canonicalize_skill_read_zones(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .filter_map(|p| match std::fs::canonicalize(p) {
            Ok(canon) => Some(canon),
            Err(e) => {
                tracing::warn!(
                    path = %p.display(),
                    err = %e,
                    "skipping skill_read_zone (canonicalize failed; fail-closed)",
                );
                None
            }
        })
        .collect()
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

    // -------- PR-A: skill_read_zones --------

    /// Read paths inside a registered skill_dir classify as
    /// [`PathClassification::InSkillDir`]. This is the lexical
    /// classification check — `tools/mod.rs::resolve_for_scope`
    /// layers the canonicalize-both-sides treatment and the
    /// read/write split on top.
    #[test]
    fn read_file_from_allowlisted_skill_dir_classifies_in_skill_dir() {
        let workspace = abs("/octos/profiles/dspfac/data");
        let skill_dir = abs("/octos/plugins/mofa-podcast");
        let scope = mt_default(&workspace, "web-1")
            .with_skill_read_zones(vec![skill_dir.clone()])
            .expect("absolute skill_dir is valid");
        assert_eq!(
            scope.classify_lexical_path(&skill_dir.join("SKILL.md")),
            PathClassification::InSkillDir {
                skill_dir: skill_dir.clone()
            }
        );
        assert_eq!(
            scope.classify_lexical_path(&skill_dir.join("data/example.json")),
            PathClassification::InSkillDir { skill_dir }
        );
    }

    /// A path that resolves outside the skill_dir via `..` traversal
    /// must classify as `OutOfScope`, never `InSkillDir`. The
    /// lexical normaliser rejects `..` outright per the existing
    /// contract — this pins that behaviour for skill_read_zones too.
    #[test]
    fn traversal_attempt_in_skill_dir_classifies_out_of_scope() {
        let workspace = abs("/octos/profiles/dspfac/data");
        let skill_dir = abs("/octos/plugins/mofa-podcast");
        let scope = mt_default(&workspace, "web-1")
            .with_skill_read_zones(vec![skill_dir.clone()])
            .unwrap();
        // Lexical `..` is refused at the normaliser layer, regardless
        // of whether the canonical destination would land inside the
        // skill_dir.
        let traversal = skill_dir.join("../../../etc/passwd");
        assert_eq!(
            scope.classify_lexical_path(&traversal),
            PathClassification::OutOfScope
        );
    }

    /// Multiple skill_dirs all participate — the classifier returns
    /// the matching dir, not the first/last entry by chance.
    #[test]
    fn multiple_skill_dirs_all_classify_in_skill_dir() {
        let workspace = abs("/octos/profiles/dspfac/data");
        let dirs = [
            abs("/octos/plugins/mofa-podcast"),
            abs("/octos/plugins/mofa-research"),
            abs("/octos/plugins/mofa-slides"),
        ];
        let scope = mt_default(&workspace, "web-1")
            .with_skill_read_zones(dirs.to_vec())
            .unwrap();
        for dir in &dirs {
            assert_eq!(
                scope.classify_lexical_path(&dir.join("SKILL.md")),
                PathClassification::InSkillDir {
                    skill_dir: dir.clone()
                },
                "expected InSkillDir match for {}",
                dir.display()
            );
        }
    }

    /// Degenerate case: a skill_dir that happens to live inside the
    /// workspace must still classify as `InWorkspace`. Workspace
    /// takes precedence so writes to the dir keep working (writes to
    /// `InSkillDir` are refused, writes to `InWorkspace` succeed).
    #[test]
    fn workspace_path_still_wins_over_skill_dir() {
        let workspace = abs("/octos/profiles/dspfac/data");
        // Pretend a skill dir was registered inside the workspace
        // tree (e.g. someone moved a skill_dir into
        // `<data>/users/web-1/workspace/skill-copy/`).
        let session_workspace = workspace.join("users/web-1/workspace");
        let nested_skill = session_workspace.join("skill-copy");
        let scope = mt_default(&workspace, "web-1")
            .with_skill_read_zones(vec![nested_skill.clone()])
            .unwrap();
        let inner = nested_skill.join("manifest.json");
        assert_eq!(
            scope.classify_lexical_path(&inner),
            PathClassification::InWorkspace,
            "workspace must outrank skill_read_zones even when a skill_dir is nested inside it",
        );
    }

    /// Solo mode also accepts skill_read_zones (the SKILL.md
    /// auto-inject fires regardless of scope mode, so solo callers
    /// need the same allowlist).
    #[test]
    fn solo_with_skill_read_zones_classifies_in_skill_dir() {
        let cwd = abs("/home/yc/my-project");
        let skill_dir = abs("/opt/octos/plugins/mofa-podcast");
        let scope = SessionScope::solo(cwd, vec![])
            .unwrap()
            .with_skill_read_zones(vec![skill_dir.clone()])
            .unwrap();
        assert_eq!(
            scope.classify_lexical_path(&skill_dir.join("SKILL.md")),
            PathClassification::InSkillDir { skill_dir }
        );
    }

    /// Solo `granted_dirs` must outrank `skill_read_zones`. Grants
    /// are explicit user approvals and carry full read+write rights;
    /// skill dirs are read-only. If a skill_dir overlapped with a
    /// granted dir, a path inside both would classify as
    /// `InGrantedDir` (read+write) rather than `InSkillDir` (read
    /// only).
    #[test]
    fn solo_granted_dir_outranks_skill_read_zone() {
        let cwd = abs("/home/yc/my-project");
        let shared_parent = abs("/opt/octos/plugins");
        let nested = shared_parent.join("mofa-podcast");
        let scope = SessionScope::solo(cwd, vec![shared_parent.clone()])
            .unwrap()
            .with_skill_read_zones(vec![nested.clone()])
            .unwrap();
        assert_eq!(
            scope.classify_lexical_path(&nested.join("SKILL.md")),
            PathClassification::InGrantedDir {
                granted_dir: shared_parent
            }
        );
    }

    /// Multi-tenant: skill_read_zones consult AFTER workspace but
    /// BEFORE shared_zones. If a skill_dir overlapped with a shared
    /// zone, the skill_dir wins. The classification order matters
    /// because writes are refused for both, but `InSharedZone` carries
    /// the cross-session-zone semantics from the host while
    /// `InSkillDir` is plugin-install material.
    #[test]
    fn skill_read_zone_outranks_shared_zone() {
        let data = abs("/octos/profiles/dspfac/data");
        let shared_research = data.join("research");
        let nested_skill = shared_research.join("mofa-podcast");
        let scope = mt_default(&data, "web-1")
            .with_skill_read_zones(vec![nested_skill.clone()])
            .unwrap();
        assert_eq!(
            scope.classify_lexical_path(&nested_skill.join("SKILL.md")),
            PathClassification::InSkillDir {
                skill_dir: nested_skill
            }
        );
    }

    /// Empty `skill_read_zones` (the default) leaves classification
    /// behaviour identical to v1 — `InSkillDir` is never returned
    /// and pre-PR-A callers see no observable change.
    #[test]
    fn empty_skill_read_zones_preserves_v1_classification() {
        let data = abs("/octos/profiles/dspfac/data");
        let scope = mt_default(&data, "web-1");
        assert!(scope.skill_read_zones().is_empty());
        // Pick a path that would have been `InSkillDir` if the zone
        // had been registered.
        let outside = abs("/octos/plugins/mofa-podcast/SKILL.md");
        assert_eq!(
            scope.classify_lexical_path(&outside),
            PathClassification::OutOfScope
        );
    }

    #[test]
    fn with_skill_read_zones_refuses_relative_paths() {
        let scope = mt_default(&abs("/octos/profiles/dspfac/data"), "web-1");
        let err = scope
            .with_skill_read_zones(vec![PathBuf::from("relative/skill")])
            .unwrap_err();
        assert!(
            matches!(err, SessionScopeError::SkillReadZoneNotAbsolute(0, _)),
            "expected SkillReadZoneNotAbsolute, got {err:?}"
        );
    }

    #[test]
    fn schema_version_bumped_to_v2_for_skill_read_zones() {
        // Pin the PR-A bump so a future PR cannot silently revert
        // the schema version without updating this test (and the
        // module-level history comment on the constant).
        assert_eq!(SESSION_SCOPE_SCHEMA_VERSION, 2);
    }

    // -----------------------------------------------------------------
    // Codex round-2 BLOCKER 2 (PR #1327 review): canonicalize-then-skip
    // helper. The pre-fix loop kept the raw path when canonicalize
    // failed; that was fail-open — a later symlink replacement
    // (`/tmp/missing -> /etc`) would canonicalise both candidate and
    // zone root to `/etc` at classify time and accept reads.
    // -----------------------------------------------------------------

    #[test]
    fn canonicalize_skill_read_zones_skips_missing_paths() {
        // Two inputs: one real on-disk dir, one missing path. The fail-
        // closed helper must KEEP the real dir and SKIP the missing
        // one. The pre-fix code kept the missing path in raw form,
        // which was fail-open.
        let real = tempfile::tempdir().expect("create real skill dir");
        let missing = real.path().join("does-not-exist");
        let input = vec![real.path().to_path_buf(), missing.clone()];
        let out = canonicalize_skill_read_zones(&input);
        assert_eq!(out.len(), 1, "missing entry must be dropped: out = {out:?}");
        let canonical_real = std::fs::canonicalize(real.path()).expect("canonicalize real");
        assert_eq!(out[0], canonical_real);
    }

    #[test]
    fn canonicalize_skill_read_zones_handles_all_missing_paths() {
        // Edge case: every entry fails canonicalize. Helper must
        // return an empty vec (no fail-open fallbacks). Caller then
        // constructs the scope with empty `skill_read_zones`, which
        // is the safe state.
        let parent = tempfile::tempdir().expect("create parent dir");
        let input = vec![parent.path().join("ghost-a"), parent.path().join("ghost-b")];
        let out = canonicalize_skill_read_zones(&input);
        assert!(
            out.is_empty(),
            "every entry missing must drop them all: out = {out:?}"
        );
    }

    #[test]
    fn canonicalize_skill_read_zones_preserves_order_when_all_present() {
        // Order-stability test: helper drops failures but preserves
        // input order for surviving entries. Callers (the scope
        // builders) rely on the classifier consulting zones in
        // declaration order.
        let a = tempfile::tempdir().expect("a");
        let b = tempfile::tempdir().expect("b");
        let c = tempfile::tempdir().expect("c");
        let input = vec![
            a.path().to_path_buf(),
            b.path().to_path_buf(),
            c.path().to_path_buf(),
        ];
        let out = canonicalize_skill_read_zones(&input);
        assert_eq!(out.len(), 3, "no entries should drop: out = {out:?}");
        let canon_a = std::fs::canonicalize(a.path()).expect("canon a");
        let canon_b = std::fs::canonicalize(b.path()).expect("canon b");
        let canon_c = std::fs::canonicalize(c.path()).expect("canon c");
        assert_eq!(out, vec![canon_a, canon_b, canon_c]);
    }

    // -----------------------------------------------------------------
    // Codex round-2 BLOCKER 1 (PR #1327 review): canonical classify
    // exposed as a SessionScope method so plugin tools and file tools
    // share one symlink-safe gate.
    // -----------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn classify_canonical_path_refuses_symlink_escape_from_skill_dir() {
        // Build:
        //   skill_dir/link -> outside/
        //   outside/                  (target of the symlink)
        //   classify <skill_dir>/link/secret must NOT be InSkillDir;
        //   canonical classify resolves through `link` and lands at
        //   `outside/secret`, which is outside the zone => OutOfScope.
        let workspace = tempfile::tempdir().expect("workspace");
        let skill_dir = tempfile::tempdir().expect("skill_dir");
        let outside = tempfile::tempdir().expect("outside");
        let link = skill_dir.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");

        // Use the *canonical* skill_dir on both sides so the lexical
        // prefix actually matches before canonicalisation. On macOS the
        // tmpdir lives under `/var/folders/...` which canonicalises to
        // `/private/var/folders/...`; if the skill_dir we configure
        // doesn't share the candidate's lexical prefix, the lexical
        // classify already returns OutOfScope and we can't pin the
        // regression scenario.
        let canonical_skill = std::fs::canonicalize(skill_dir.path()).expect("canon skill");
        let workspace_canon = std::fs::canonicalize(workspace.path()).expect("canon workspace");
        let canonical_link = canonical_skill.join("link");
        let candidate = canonical_link.join("secret");

        let scope = SessionScope::solo(workspace_canon, vec![])
            .expect("build solo scope")
            .with_skill_read_zones(vec![canonical_skill.clone()])
            .expect("attach skill_read_zone");

        // Lexical classify accepts because the lexical path is
        // `<skill_dir>/link/secret` and `link` is a Normal component
        // before the canonical walk happens. Pin that this is the
        // exact regression scenario.
        assert!(
            matches!(
                scope.classify_lexical_path(&candidate),
                PathClassification::InSkillDir { .. }
            ),
            "lexical classify wrongly accepts symlink escape — the bug we're fixing",
        );
        // Canonical classify must refuse.
        assert_eq!(
            scope.classify_canonical_path(&candidate),
            PathClassification::OutOfScope,
            "canonical classify must refuse <skill_dir>/symlink/<file> when the symlink escapes",
        );
    }

    #[cfg(unix)]
    #[test]
    fn classify_canonical_path_accepts_real_skill_dir_files() {
        // Positive baseline: when no symlinks are involved, canonical
        // classify must still accept files under a skill_dir.
        let workspace = tempfile::tempdir().expect("workspace");
        let skill_dir = tempfile::tempdir().expect("skill_dir");
        let manifest = skill_dir.path().join("SKILL.md");
        std::fs::write(&manifest, b"# fixture").unwrap();
        let canonical_skill = std::fs::canonicalize(skill_dir.path()).expect("canon skill");
        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
            .expect("build solo scope")
            .with_skill_read_zones(vec![canonical_skill.clone()])
            .expect("attach skill_read_zone");
        match scope.classify_canonical_path(&manifest) {
            PathClassification::InSkillDir { skill_dir: dir } => {
                assert_eq!(dir, canonical_skill, "report the configured skill_dir form");
            }
            other => panic!("expected InSkillDir for real skill_dir file, got {other:?}"),
        }
    }
}
