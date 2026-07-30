//! [`WorkerGrant`] — the operator-supplied capability grant for one fleet
//! task-worker.
//!
//! PR A of the fleet kernel replaces the worker's HARDCODED permissions (a
//! fixed closed registry + `allow_network=false`) with an explicit grant the
//! master provisions per task, exactly as a human operator would: which
//! network it may reach (per-host allowlist), which tools it may hold, and
//! which filesystem paths it may touch. The default is **least privilege** —
//! an unspecified grant is [`WorkerGrant::minimal`], byte-for-byte today's
//! closed worker (no network, the base file tools, workspace-write) — and the
//! master EXPANDS it explicitly, per task.
//!
//! This type is deliberately a **plain serde value** with no baked-in
//! immutability: PR B (mid-task escalation) will let a worker request more and
//! the master REPLACE the grant with a wider one; nothing here forecloses that.
//!
//! The type lives in `octos-fleet` (zero `octos-agent` dependency) so it can
//! persist on the durable [`crate::PlanTask`]. The host-side realisation — how
//! `fs` maps to `EffectivePermissions` and how a tool name is built — lives in
//! `octos-fleet-worker`, which owns the tool catalog.

use serde::{Deserialize, Serialize};

/// The base replay-safe file tools every closed worker holds by default —
/// today's `ALLOWED` seven. Each is idempotent-enough to re-run headless and
/// none blocks on human input. [`WorkerGrant::minimal`] grants exactly these.
pub const BASE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob",
    "grep",
    "list_dir",
    "shell",
];

/// Every tool a master MAY grant a worker in PR A: the base file tools plus the
/// two network content tools. A grant naming anything outside this catalog is
/// rejected at validation — the operator cannot grant a tool the host cannot
/// build. Extensible (message/etc.) in later PRs.
pub const GRANTABLE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob",
    "grep",
    "list_dir",
    "shell",
    "web_fetch",
    "web_search",
];

/// The network content tools — buildable ONLY under a network grant (`Hosts`
/// or `Full`). Granting one under [`NetworkGrant::None`] is an incoherent grant
/// (a network tool with no network) and is rejected at validation.
pub const WEB_TOOLS: &[&str] = &["web_fetch", "web_search"];

/// The network egress a worker is granted.
///
/// - [`NetworkGrant::None`] — zero network: the sandbox blocks egress and no
///   web tool is built.
/// - [`NetworkGrant::Hosts`] — the sandbox still blocks RAW egress (the shell
///   cannot `curl`); the only network path is the granted web tools, which
///   enforce this per-host allowlist (on top of the private-IP block). This is
///   how a per-host allowance is made REAL rather than cosmetic.
/// - [`NetworkGrant::Full`] — raw egress on (`allow_network=true`): the shell
///   can reach the network (git/npm/etc.) and web tools are unrestricted (the
///   private-IP block still applies).
///
/// **v1 limitation (raw network is all-or-nothing).** Per-host filtering of
/// RAW network (e.g. `git clone`/`npm` to only certain hosts) needs an egress
/// proxy the kernel does not yet run. `Hosts` therefore covers HTTP(S) via the
/// web tools ONLY; `Full` is unfiltered raw egress. Documented, not solved in
/// PR A.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkGrant {
    #[default]
    None,
    Hosts(Vec<String>),
    Full,
}

impl NetworkGrant {
    /// Whether the SANDBOX is given raw egress (`allow_network`). Only `Full`.
    /// `Hosts` deliberately keeps raw egress OFF — its allowance is realised
    /// solely through the granted web tools' host allowlist, so the shell can
    /// never `curl` past the allowlist. `None` is off.
    pub fn allows_raw_egress(&self) -> bool {
        matches!(self, NetworkGrant::Full)
    }

    /// The host allowlist a granted web tool must enforce, if any.
    /// `Hosts(hosts)` → `Some(hosts)`; `Full` → `None` (unrestricted, the
    /// private-IP block still applies); `None` → `None` (no web tool is built).
    pub fn web_allowlist(&self) -> Option<&[String]> {
        match self {
            NetworkGrant::Hosts(hosts) => Some(hosts),
            _ => None,
        }
    }

    /// Whether this grant permits building the web (network) tools at all.
    pub fn permits_web_tools(&self) -> bool {
        !matches!(self, NetworkGrant::None)
    }
}

/// The filesystem reach a worker is granted.
///
/// **Deliberately COARSE in v1.** The native file tools' confinement is a
/// binary [`crate::policy`-style] scope (`Workspace | Host`) with no per-path
/// allowlist, so an honest grant is binary too:
///
/// - [`FsGrant::Workspace`] (the default) — the worker may read+write ONLY its
///   own attempt working directory. This is byte-for-byte today's closed
///   worker.
/// - [`FsGrant::Host`] — the worker may read+write the WHOLE daemon-user
///   filesystem (fleet data/config included). This is a broad, EXPLICIT
///   operator choice — grant it only when a task genuinely needs host access.
///
/// **v1 limitation (no per-path FS scoping).** A narrow "these specific paths,
/// distinct read vs write" grant is NOT expressible against the binary native
/// scope, so it is a FOLLOW-UP (it needs a native-tool path-allowlist model,
/// exactly like per-host filtering of raw network needs an egress proxy). This
/// enum does not PRETEND to offer narrow paths — `Host` is all-or-nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsGrant {
    /// Confined to the worker's own cwd (read+write). The least-privilege
    /// default.
    #[default]
    Workspace,
    /// Full daemon-user filesystem read+write. An explicit, broad operator
    /// grant.
    Host,
}

impl FsGrant {
    /// Whether this grant reaches beyond the worker's own cwd (full host r/w).
    pub fn is_host(&self) -> bool {
        matches!(self, FsGrant::Host)
    }
}

/// The whole operator grant for one worker: network + tools + filesystem.
///
/// `#[serde(default)]` on every field means an OLD persisted task (written
/// before grants existed) — or a master that specifies nothing — loads as
/// [`WorkerGrant::minimal`], preserving least-privilege by default and keeping
/// old records readable without a schema bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerGrant {
    /// Network egress. Default [`NetworkGrant::None`].
    #[serde(default)]
    pub network: NetworkGrant,
    /// The tools the worker may hold. Default [`BASE_TOOLS`].
    #[serde(default = "base_tools_vec")]
    pub tools: Vec<String>,
    /// Filesystem reach. Default [`FsGrant::Workspace`] (cwd-only).
    #[serde(default)]
    pub fs: FsGrant,
}

fn base_tools_vec() -> Vec<String> {
    BASE_TOOLS.iter().map(|s| (*s).to_string()).collect()
}

impl Default for WorkerGrant {
    fn default() -> Self {
        Self::minimal()
    }
}

impl WorkerGrant {
    /// The least-privilege grant == today's closed worker: no network, the
    /// base file tools, workspace-write (cwd only). This is what an unspecified
    /// grant resolves to, so every pre-grant dispatch path stays byte-for-byte
    /// identical.
    pub fn minimal() -> Self {
        Self {
            network: NetworkGrant::None,
            tools: base_tools_vec(),
            fs: FsGrant::default(),
        }
    }

    /// The granted tool names, deduplicated + sorted — the closed-worker audit
    /// key. The built registry's `tool_names()` must equal this exactly ("what
    /// the operator granted, nothing more"). Dedup matches the registry, which
    /// keys tools by name, so a grant that repeats a name still audits cleanly.
    pub fn sorted_tools(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.clone();
        names.sort();
        names.dedup();
        names
    }

    /// Reject a grant the host cannot honor:
    /// - a tool outside [`GRANTABLE_TOOLS`] (the host cannot build it),
    /// - a web tool under [`NetworkGrant::None`] (a network tool with no
    ///   network is incoherent — grant `Hosts`/`Full` or drop the tool), and
    /// - an EMPTY [`NetworkGrant::Hosts`] allowlist (an operator that lists no
    ///   hosts means [`NetworkGrant::None`]; accepting it would be a fail-OPEN
    ///   trap — an empty allowlist must never read as "unrestricted").
    ///
    /// Called at parse time (the master's `goal_plan`) AND defensively at
    /// registry-build time, so an incoherent grant can never reach a live
    /// worker.
    pub fn validate(&self) -> Result<(), GrantError> {
        if let NetworkGrant::Hosts(hosts) = &self.network {
            if hosts.iter().all(|h| h.trim().is_empty()) {
                return Err(GrantError::EmptyHostAllowlist);
            }
        }
        for tool in &self.tools {
            if !GRANTABLE_TOOLS.contains(&tool.as_str()) {
                return Err(GrantError::UnknownTool(tool.clone()));
            }
        }
        if !self.network.permits_web_tools() {
            for tool in &self.tools {
                if WEB_TOOLS.contains(&tool.as_str()) {
                    return Err(GrantError::WebToolWithoutNetwork(tool.clone()));
                }
            }
        }
        Ok(())
    }
}

/// A typed rejection of an incoherent [`WorkerGrant`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// A granted tool is not in [`GRANTABLE_TOOLS`].
    UnknownTool(String),
    /// A web tool was granted under [`NetworkGrant::None`].
    WebToolWithoutNetwork(String),
    /// A [`NetworkGrant::Hosts`] grant with an empty allowlist (fail-open trap).
    EmptyHostAllowlist,
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrantError::UnknownTool(tool) => write!(
                f,
                "tool `{tool}` is not grantable (must be one of: {})",
                GRANTABLE_TOOLS.join(", ")
            ),
            GrantError::WebToolWithoutNetwork(tool) => write!(
                f,
                "tool `{tool}` needs a network grant — set network to `hosts` (allowlist) \
                 or `full`, or drop the tool"
            ),
            GrantError::EmptyHostAllowlist => write!(
                f,
                "network mode `hosts` needs a non-empty allowlist — an empty list denies \
                 everything; use `none` for no network"
            ),
        }
    }
}

impl std::error::Error for GrantError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_minimal_is_todays_closed_worker() {
        let g = WorkerGrant::minimal();
        assert_eq!(g.network, NetworkGrant::None);
        assert!(!g.network.allows_raw_egress(), "minimal has no raw egress");
        assert!(g.network.web_allowlist().is_none());
        // Exactly the base seven, in order.
        assert_eq!(g.tools, BASE_TOOLS);
        assert_eq!(g.fs, FsGrant::Workspace, "minimal is workspace-only");
        assert!(!g.fs.is_host());
        // Default == minimal (drives `#[serde(default)]`).
        assert_eq!(WorkerGrant::default(), g);
        g.validate().expect("minimal is always valid");
    }

    #[test]
    fn grant_missing_fields_default_to_minimal() {
        // A master (or an old record) that specifies nothing gets least
        // privilege. Serde `default` fills every field.
        let g: WorkerGrant = serde_json::from_str("{}").unwrap();
        assert_eq!(g, WorkerGrant::minimal());
        // Partial: only tools set → network + fs default.
        let g: WorkerGrant = serde_json::from_str(r#"{"tools":["read_file","glob"]}"#).unwrap();
        assert_eq!(g.network, NetworkGrant::None);
        assert_eq!(g.tools, vec!["read_file".to_string(), "glob".to_string()]);
        assert_eq!(g.fs, FsGrant::Workspace);
    }

    #[test]
    fn grant_unknown_tool_is_rejected() {
        let g = WorkerGrant {
            tools: vec!["read_file".into(), "definitely_not_a_tool".into()],
            ..WorkerGrant::minimal()
        };
        assert_eq!(
            g.validate(),
            Err(GrantError::UnknownTool("definitely_not_a_tool".into())),
        );
    }

    #[test]
    fn grant_web_tool_without_network_is_rejected() {
        // A network tool under NetworkGrant::None is incoherent.
        let g = WorkerGrant {
            network: NetworkGrant::None,
            tools: vec!["read_file".into(), "web_fetch".into()],
            ..WorkerGrant::minimal()
        };
        assert_eq!(
            g.validate(),
            Err(GrantError::WebToolWithoutNetwork("web_fetch".into())),
        );
        // Same tool under Hosts/Full is fine.
        let hosts = WorkerGrant {
            network: NetworkGrant::Hosts(vec!["example.com".into()]),
            tools: vec!["read_file".into(), "web_fetch".into()],
            ..WorkerGrant::minimal()
        };
        hosts.validate().expect("web_fetch under Hosts is valid");
    }

    #[test]
    fn grant_empty_hosts_allowlist_is_rejected() {
        // A `Hosts` grant with an empty allowlist is a fail-OPEN trap (an empty
        // list must never read as "unrestricted"). validate() rejects it — the
        // operator meant `None`.
        let empty = WorkerGrant {
            network: NetworkGrant::Hosts(vec![]),
            tools: vec!["read_file".into(), "web_fetch".into()],
            ..WorkerGrant::minimal()
        };
        assert_eq!(empty.validate(), Err(GrantError::EmptyHostAllowlist));
        // A list of only blanks is equally empty.
        let blanks = WorkerGrant {
            network: NetworkGrant::Hosts(vec!["  ".into()]),
            tools: vec!["read_file".into()],
            ..WorkerGrant::minimal()
        };
        assert_eq!(blanks.validate(), Err(GrantError::EmptyHostAllowlist));
    }

    #[test]
    fn network_grant_egress_and_allowlist_semantics() {
        assert!(!NetworkGrant::None.allows_raw_egress());
        assert!(!NetworkGrant::Hosts(vec!["example.com".into()]).allows_raw_egress());
        assert!(NetworkGrant::Full.allows_raw_egress(), "Full = raw egress");

        assert_eq!(
            NetworkGrant::Hosts(vec!["a.com".into(), "b.com".into()]).web_allowlist(),
            Some(&["a.com".to_string(), "b.com".to_string()][..]),
        );
        assert!(
            NetworkGrant::Full.web_allowlist().is_none(),
            "Full = unrestricted"
        );
        assert!(NetworkGrant::None.web_allowlist().is_none());
    }

    #[test]
    fn grant_round_trips_through_json() {
        let g = WorkerGrant {
            network: NetworkGrant::Hosts(vec!["example.com".into()]),
            tools: vec!["read_file".into(), "shell".into(), "web_fetch".into()],
            fs: FsGrant::Host,
        };
        let json = serde_json::to_string(&g).unwrap();
        let back: WorkerGrant = serde_json::from_str(&json).unwrap();
        assert_eq!(g, back);
    }

    #[test]
    fn sorted_tools_dedups_and_sorts() {
        let g = WorkerGrant {
            tools: vec!["shell".into(), "read_file".into(), "shell".into()],
            ..WorkerGrant::minimal()
        };
        assert_eq!(
            g.sorted_tools(),
            vec!["read_file".to_string(), "shell".to_string()]
        );
    }
}
