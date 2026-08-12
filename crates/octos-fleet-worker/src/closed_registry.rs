//! Module 1 — the operator-granted, replay-safe worker tool registry (**the
//! crux**).
//!
//! PR A: a fleet task-worker no longer holds a HARDCODED closed set. The master
//! provisions each worker's capabilities explicitly at dispatch as a
//! [`WorkerGrant`] — network, tools, filesystem — and the host builds the
//! worker FROM that grant. The DEFAULT is least privilege:
//! [`WorkerGrant::minimal`] is byte-for-byte the old closed worker (no network,
//! the base seven file tools, workspace-write), so every pre-grant dispatch
//! path is unchanged.
//!
//! A worker is still *provably* bounded to exactly what it was granted:
//! [`build_fleet_worker_registry`] starts from an EMPTY [`ToolRegistry`] (never
//! [`ToolRegistry::with_builtins`], which registers ~35 tools including the
//! parking + fan-out set), registers exactly the granted tools from a KNOWN
//! CATALOG, and then hard-removes anything else with
//! [`ToolRegistry::apply_policy`]. The exhaustive audit test asserts
//! `tool_names() == grant.sorted_tools()`, so the closed-worker guarantee now
//! reads "exactly what the operator granted, nothing more" — any future dynamic
//! (MCP/plugin/skill) tool sneaking in fails the build.

use std::path::Path;
use std::sync::Arc;

use eyre::{Result, eyre};
use octos_agent::policy::{ApprovalPolicy, EffectivePermissions, FilesystemScope};
use octos_agent::sandbox::Sandbox;
use octos_agent::tools::policy::ToolPolicy;
use octos_agent::tools::write_grant::{WriteGrantViolationSink, WritePathGrant};
use octos_agent::tools::{
    EditFileTool, GlobTool, GrepTool, ListDirTool, ReadFileTool, ShellTool, ToolRegistry,
    WebFetchTool, WebSearchTool, WriteFileTool,
};
use octos_fleet::WorkerGrant;

use crate::escalate::{EscalateTool, EscalationSlot};

/// The base tool set a *minimal* (least-privilege) worker holds — today's
/// closed seven. Kept as a named constant for docs / discriminator tests;
/// equals [`octos_fleet::BASE_TOOLS`] and `WorkerGrant::minimal().tools`. The
/// audit no longer compares against this fixed set — it compares against the
/// per-worker `grant.sorted_tools()`.
pub const ALLOWED: &[&str] = octos_fleet::BASE_TOOLS;

/// Map a [`WorkerGrant`]'s filesystem grant onto [`EffectivePermissions`].
///
/// [`FsGrant::Workspace`] (the minimal default) maps to
/// `FilesystemScope::Workspace` with `ReadWrite`, reproducing today's
/// `workspace_write()` closed worker exactly (cwd-only). [`FsGrant::Host`] maps
/// to `FilesystemScope::Host` (full daemon-user read+write) — an explicit,
/// broad operator grant.
///
/// **v1 limitation (coarse fs scope, honestly binary).** The native file tools'
/// scope IS binary (`Workspace | Host`) with no per-path allowlist, so the grant
/// is binary too — there is no silent "some paths" middle ground that would
/// promise narrow access but deliver host-wide. A narrow per-path FS grant is a
/// FOLLOW-UP (it needs a native-tool path-allowlist model), exactly like
/// per-host filtering of raw network needs an egress proxy.
///
/// [`FsGrant::Workspace`]: octos_fleet::FsGrant::Workspace
/// [`FsGrant::Host`]: octos_fleet::FsGrant::Host
fn perms_from_grant(grant: &WorkerGrant) -> EffectivePermissions {
    let mut perms =
        EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never);
    if grant.fs.is_host() {
        perms.filesystem_scope = FilesystemScope::Host;
    }
    perms
}

/// Build the replay-safe tool registry for a fleet task-worker rooted at `cwd`,
/// FROM the operator's [`WorkerGrant`], with `sandbox` backing the shell tool
/// and each shell command's effective timeout CAPPED at `max_shell_timeout_secs`
/// (the attempt deadline, in whole seconds) so no foreground command outlives
/// it.
///
/// The registry holds EXACTLY the granted tools — the base file tools (scoped
/// by `grant.fs`) plus, if granted, the network content tools (`web_fetch` /
/// `web_search`). A tool outside the grantable catalog, or a web tool with no
/// network grant, is a hard error (`grant.validate()`), so an incoherent grant
/// can never produce a live worker.
///
/// The granted tool set is a DENYLIST at the tool boundary (it omits
/// parking/fan-out/etc.), NOT a network or process boundary. Under a `None` /
/// `Hosts` grant the surviving `shell` has NO network (the sandbox blocks raw
/// egress; the ONLY network path is the granted web tools, restricted to the
/// allowlist). Under a `Full` grant the shell reaches the network and can
/// detach children via shell-internal backgrounding that string inspection
/// cannot catch — both bounded by the **sandbox**. Passing a no-op sandbox here
/// is an operator error (flagged with a `tracing::warn!`), analogous to
/// `--danger-full-access`.
pub fn build_fleet_worker_registry(
    cwd: &Path,
    sandbox: Arc<dyn Sandbox>,
    max_shell_timeout_secs: u64,
    grant: &WorkerGrant,
    escalation: EscalationSlot,
    // #1976 — optional `[denied]`-violation audit sink. `Some` on the AGENT
    // path (the fleet host wires it to the goal ledger); `None` on the
    // acceptance-validator path (validators never call file tools) and in the
    // pure unit tests. The tool refusal is returned to the model regardless;
    // the sink is the DURABLE audit trail on top of it.
    violation_sink: Option<WriteGrantViolationSink>,
) -> Result<ToolRegistry> {
    // Reject an incoherent grant up front (unknown tool / web tool with no
    // network, or an incoherent per-path write fence — #1976) — validated at
    // parse time too, so this is defense-in-depth: an unknown tool or an
    // inexpressible fence can never reach a live worker.
    grant
        .validate()
        .map_err(|e| eyre!("fleet worker: invalid grant: {e}"))?;

    // #1976 — build the per-path write fence from the grant's `write_paths`
    // (`None` = no fence, byte-for-byte the pre-#1976 worker). Compiled ONCE
    // here and cloned onto write_file + edit_file so both enforce the same
    // allowlist. A pattern the fleet-side `validate()` already accepted must
    // compile here too; a mismatch fails the build (fail closed) rather than
    // silently dropping the fence.
    let write_fence: Option<WritePathGrant> = match &grant.write_paths {
        Some(paths) => {
            let mut fence = WritePathGrant::new(paths, grant.create_only)
                .map_err(|e| eyre!("fleet worker: invalid write grant: {e}"))?;
            if let Some(sink) = violation_sink.clone() {
                fence = fence.with_violation_sink(sink);
            }
            Some(fence)
        }
        None => None,
    };

    // P1-3-enforce (document, don't type-enforce): the API cannot police
    // sandbox quality, but a no-op sandbox leaves the shell's network reach and
    // detached children unbounded — surface it so it can't pass silently.
    if sandbox.is_noop() {
        tracing::warn!(
            "fleet worker: building a granted registry with a NO-OP sandbox — the \
             shell's network reach and detached children are UNBOUNDED; production \
             must supply a network-isolated sandbox",
        );
    }

    // Filesystem reach from the grant. Approvals FAIL CLOSED at the tool
    // boundary: a shell command the SafePolicy would ask about is denied
    // outright (a closed worker has no human to ask). `SafePolicy` is preserved
    // — we do NOT widen to AllowAll — so dangerous commands stay blocked.
    let perms = perms_from_grant(grant);
    let scope = perms.filesystem_scope; // Workspace (minimal) or Host (fs=Host)
    let access = perms.file_access; // ReadWrite

    let mut r = ToolRegistry::new();
    // Mirror `with_builtins`: record the workspace root so the agent's
    // session-scope fallback and any project-root machinery resolve against
    // the same cwd the tools are bound to.
    r.set_workspace_root(cwd.to_path_buf());

    // Build each granted tool from the KNOWN CATALOG. An unknown name is
    // unreachable after `validate()` above, but the arm returns a hard error
    // defensively rather than silently dropping a tool.
    for name in &grant.tools {
        match name.as_str() {
            "shell" => r.register(
                ShellTool::new(cwd)
                    .with_shared_sandbox(sandbox.clone())
                    .with_policy(perms.shell_command_policy())
                    .with_approval_policy(ApprovalPolicy::Never)
                    // Refuse the string-detectable detach vectors (`background:
                    // true` and a trailing `&`) as defense-in-depth — the
                    // sandbox is the real boundary for detached children.
                    .with_background_allowed(false)
                    // Hard per-command CEILING at the attempt deadline: combined
                    // with `background_allowed(false)`, no single foreground
                    // command outlives the deadline even if the LLM requests a
                    // larger `timeout_secs`.
                    .with_max_timeout_secs(max_shell_timeout_secs),
            ),
            "read_file" => r.register(ReadFileTool::new(cwd).with_filesystem_scope(scope)),
            // #1976 — write_file / edit_file carry the per-path write fence
            // when granted (deny-wins on top of the fs scope). Under
            // `create_only` write_file opens `O_CREAT|O_EXCL` and edit_file
            // is refused outright — enforced inside the tools.
            "write_file" => {
                let mut tool = WriteFileTool::new(cwd)
                    .with_filesystem_scope(scope)
                    .with_file_access(access);
                if let Some(fence) = &write_fence {
                    tool = tool.with_write_grant(fence.clone());
                }
                r.register(tool);
            }
            "edit_file" => {
                let mut tool = EditFileTool::new(cwd)
                    .with_filesystem_scope(scope)
                    .with_file_access(access);
                if let Some(fence) = &write_fence {
                    tool = tool.with_write_grant(fence.clone());
                }
                r.register(tool);
            }
            "glob" => r.register(GlobTool::new(cwd).with_filesystem_scope(scope)),
            "grep" => r.register(GrepTool::new(cwd).with_filesystem_scope(scope)),
            "list_dir" => r.register(ListDirTool::new(cwd).with_filesystem_scope(scope)),
            // The network content tools — buildable ONLY under a network grant
            // (`validate()` rejects them under `None`). `web_fetch` ENFORCES the
            // per-host allowlist (`Hosts` → the list; `Full` → unrestricted, the
            // private-IP block still applies). This is the ONLY network path
            // under `Hosts` (the shell has no raw egress there).
            "web_fetch" => {
                let tool = match grant.network.web_allowlist() {
                    Some(hosts) => WebFetchTool::new().with_host_allowlist(hosts.to_vec()),
                    None => WebFetchTool::new(),
                };
                r.register(tool);
            }
            // `web_search` targets fixed search-PROVIDER endpoints (not arbitrary
            // content hosts), so it is catalog-gated (buildable only when
            // granted) but not itself host-allowlist-filtered in v1 — content
            // retrieval remains allowlist-bound via `web_fetch` (documented v1
            // limitation).
            "web_search" => r.register(WebSearchTool::new()),
            other => {
                return Err(eyre!(
                    "fleet worker: tool `{other}` is not in the grantable catalog"
                ));
            }
        }
    }

    // PR B — the always-on `escalate` safety valve. Registered UNCONDITIONALLY,
    // AFTER the grant loop, and NOT grant-gated: even a minimal-grant worker must
    // be able to ASK for more capability when a task hits the edge of its grant.
    // It only RECORDS a request (into the shared slot) and returns — it never
    // parks (`blocks_on_human_input == false`) and never self-widens the grant
    // (only the keeper's `goal_grant` mutates `PlanTask.grant`).
    r.register(EscalateTool::new(escalation));

    // Belt-and-suspenders: hard-remove anything not in the granted allow-set
    // PLUS the always-on `escalate` valve. A no-op today (we registered exactly
    // the grant + escalate and nothing auto-swaps), but it locks the invariant so
    // a future edit that registers an extra tool cannot silently widen the
    // worker's reach — `apply_policy` -> `retain` is a real removal. The allow
    // list is never empty here (it always contains `escalate`), so it can never
    // collapse into the empty=allow-all case.
    let mut allow = grant.tools.clone();
    allow.push("escalate".to_string());
    r.apply_policy(&ToolPolicy {
        allow,
        ..Default::default()
    });
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_agent::sandbox::NoSandbox;
    use octos_fleet::{FsGrant, NetworkGrant};
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// A fresh, empty escalation slot for a test build.
    fn slot() -> EscalationSlot {
        Arc::new(Mutex::new(None))
    }

    /// The audit key for a worker registry: the granted tools PLUS the always-on
    /// `escalate` valve, deduped + sorted.
    fn sorted_with_escalate(grant: &WorkerGrant) -> Vec<String> {
        let mut names = grant.sorted_tools();
        names.push("escalate".to_string());
        names.sort();
        names.dedup();
        names
    }

    /// Tools that must NEVER appear in a closed worker registry. This is a
    /// supplementary explicit denylist; the exhaustive `tool_names() ==
    /// grant.sorted_tools()` assertion below is the real guard (it also catches
    /// names not listed here). Covers the parking, fan-out, peer, channel,
    /// memory, skill, and dispatch families.
    const FORBIDDEN: &[&str] = &[
        // parking / plan / human input
        "ask_user_question",
        "request_user_input",
        "update_plan",
        "write_stdin",
        // fan-out / sub-agent lifecycle
        "spawn",
        "spawn_agent",
        "delegate",
        "delegate_task",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
        "read_task_output",
        "check_background_tasks",
        // peers
        "peer_handoff",
        "peer_gather",
        "peer_list",
        "peer_send_input",
        "peer_close",
        "peer_respond",
        // channel / messaging
        "cron",
        "message",
        "send_file",
        "send_app_card",
        // network / external (NOT granted in a minimal worker)
        "browser",
        "search",
        "deep_crawl",
        "http",
        "synthesize_research",
        // skills / dispatch
        "manage_skills",
        "mofa_make",
        "mofa_describe_content_type",
        // memory
        "recall_memory",
        "save_memory",
        "memory_note",
        "record_memory_use",
        // misc non-replay-safe
        "view_image",
        "tool_search",
        "tool_suggest",
        "image_generation",
        // extra shell/edit variants that with_builtins also registers but the
        // closed worker deliberately excludes
        "exec_command",
        "bash",
        "apply_patch",
        "diff_edit",
    ];

    #[test]
    fn grant_minimal_reproduces_todays_closed_worker() {
        // PR A: the minimal grant is byte-for-byte the old closed worker — the
        // base seven, workspace-write, no network tools. The audit compares
        // against `grant.sorted_tools()` (== the old `ALLOWED` for minimal).
        let grant = WorkerGrant::minimal();
        let reg = build_fleet_worker_registry(
            Path::new("/tmp/fleet-worker-audit"),
            Arc::new(NoSandbox),
            30,
            &grant,
            slot(),
            None,
        )
        .expect("minimal grant builds");

        // EXHAUSTIVE: the registry contains EXACTLY the granted tools PLUS the
        // always-on `escalate` valve (PR B) — nothing more.
        let mut names = reg.tool_names();
        names.sort();
        assert_eq!(
            names,
            sorted_with_escalate(&grant),
            "a minimal-grant worker holds exactly the base replay-safe tools + escalate",
        );
        // And the GRANTED subset is the old closed seven (escalate is separate).
        assert_eq!(grant.sorted_tools(), {
            let mut v: Vec<String> = ALLOWED.iter().map(|s| s.to_string()).collect();
            v.sort();
            v
        });
        // The escalate valve is present, LLM-visible, and never parks.
        assert!(
            reg.get("escalate").is_some(),
            "escalate is always available"
        );
        assert!(
            !reg.blocks_on_human_input("escalate"),
            "escalate must NOT block on human input — it records and returns",
        );

        let spec_names: HashSet<String> = reg.specs().into_iter().map(|s| s.name).collect();

        // Every FORBIDDEN name is un-gettable AND absent from specs().
        for name in FORBIDDEN {
            assert!(
                reg.get(name).is_none(),
                "forbidden tool {name} must not be gettable from the closed registry",
            );
            assert!(
                !spec_names.contains(*name),
                "forbidden tool {name} must not appear in specs()",
            );
        }

        // NOTHING in the registry blocks on human input.
        for name in reg.tool_names() {
            assert!(
                !reg.blocks_on_human_input(&name),
                "tool {name} blocks on human input — a closed worker must never park",
            );
        }

        // Sanity: every granted tool is present + LLM-visible.
        for name in &grant.tools {
            assert!(
                reg.get(name).is_some(),
                "granted tool {name} missing from registry",
            );
            assert!(
                spec_names.contains(name),
                "granted tool {name} not exposed in specs()",
            );
        }
        // A minimal worker has NO web tools.
        assert!(reg.get("web_fetch").is_none());
        assert!(reg.get("web_search").is_none());
    }

    #[test]
    fn escalate_tool_is_always_available_even_at_minimal_grant() {
        // PR B — the safety valve is NOT grant-gated: even the least-privilege
        // worker holds `escalate`, and the audit key is `sorted_tools() +
        // escalate`. A grant that names NO extra tools still gets the valve.
        for grant in [
            WorkerGrant::minimal(),
            WorkerGrant {
                tools: vec!["read_file".into()],
                ..WorkerGrant::minimal()
            },
        ] {
            let reg = build_fleet_worker_registry(
                Path::new("/tmp/fleet-escalate-valve"),
                Arc::new(NoSandbox),
                30,
                &grant,
                slot(),
                None,
            )
            .expect("grant builds");
            assert!(
                reg.get("escalate").is_some(),
                "escalate must exist for grant {:?}",
                grant.tools,
            );
            let mut names = reg.tool_names();
            names.sort();
            assert_eq!(
                names,
                sorted_with_escalate(&grant),
                "audit = granted tools + escalate",
            );
        }
    }

    #[test]
    fn grant_expands_tools_and_scopes() {
        // A master grants +web_fetch (under a Hosts network) and Host fs → the
        // registry gains web_fetch and the native file tools' EffectivePermissions
        // widen to Host scope.
        let cwd = Path::new("/tmp/fleet-expand");
        let grant = WorkerGrant {
            network: NetworkGrant::Hosts(vec!["example.com".into()]),
            tools: {
                let mut t: Vec<String> = octos_fleet::BASE_TOOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                t.push("web_fetch".into());
                t
            },
            fs: FsGrant::Host,
            write_paths: None,
            create_only: false,
        };
        let reg = build_fleet_worker_registry(cwd, Arc::new(NoSandbox), 30, &grant, slot(), None)
            .expect("expanded grant builds");

        assert!(reg.get("web_fetch").is_some(), "granted web_fetch present");
        let mut names = reg.tool_names();
        names.sort();
        assert_eq!(
            names,
            sorted_with_escalate(&grant),
            "exactly the granted set + escalate",
        );

        // The Host fs grant widens native tools to Host scope.
        assert_eq!(
            perms_from_grant(&grant).filesystem_scope,
            FilesystemScope::Host,
            "an fs=Host grant opens Host scope for native tools",
        );
        // The minimal (Workspace) grant stays Workspace-scoped (cwd-only).
        assert_eq!(
            perms_from_grant(&WorkerGrant::minimal()).filesystem_scope,
            FilesystemScope::Workspace,
            "minimal fs stays workspace-scoped",
        );
    }

    #[test]
    fn grant_hosts_allowlist_enforced_on_web_tool() {
        // A Hosts grant builds the web tool and keeps raw egress OFF — the
        // allowlist is enforced by the web tool (tested in octos-agent), and
        // the sandbox never gets raw network under Hosts.
        let grant = WorkerGrant {
            network: NetworkGrant::Hosts(vec!["example.com".into()]),
            tools: vec!["read_file".into(), "web_fetch".into()],
            ..WorkerGrant::minimal()
        };
        let reg = build_fleet_worker_registry(
            Path::new("/tmp/fleet-hosts"),
            Arc::new(NoSandbox),
            30,
            &grant,
            slot(),
            None,
        )
        .expect("hosts grant builds");
        assert!(reg.get("web_fetch").is_some());
        assert!(
            !grant.network.allows_raw_egress(),
            "Hosts must NOT grant raw sandbox egress — the shell cannot curl",
        );
        assert_eq!(
            grant.network.web_allowlist(),
            Some(&["example.com".to_string()][..]),
            "the allowlist is threaded to the web tool",
        );
    }

    #[test]
    fn grant_full_enables_raw_network() {
        // A Full grant turns on raw sandbox egress (git/npm) and builds web
        // tools unrestricted (private-IP block still applies).
        let grant = WorkerGrant {
            network: NetworkGrant::Full,
            tools: vec!["shell".into(), "web_fetch".into()],
            ..WorkerGrant::minimal()
        };
        let reg = build_fleet_worker_registry(
            Path::new("/tmp/fleet-full"),
            Arc::new(NoSandbox),
            30,
            &grant,
            slot(),
            None,
        )
        .expect("full grant builds");
        assert!(reg.get("web_fetch").is_some());
        assert!(
            grant.network.allows_raw_egress(),
            "Full grants raw sandbox egress",
        );
        assert!(
            grant.network.web_allowlist().is_none(),
            "Full leaves web tools unrestricted (no host allowlist)",
        );
    }

    #[test]
    fn grant_unknown_tool_is_rejected_at_build() {
        // Defense-in-depth: even if an unknown tool slips past parse validation,
        // the build refuses it rather than dropping it silently.
        let grant = WorkerGrant {
            tools: vec!["read_file".into(), "definitely_not_a_tool".into()],
            ..WorkerGrant::minimal()
        };
        let result = build_fleet_worker_registry(
            Path::new("/tmp/fleet-bad"),
            Arc::new(NoSandbox),
            30,
            &grant,
            slot(),
            None,
        );
        let err = result
            .err()
            .expect("unknown tool must be rejected")
            .to_string();
        assert!(
            err.contains("definitely_not_a_tool"),
            "error names the bad tool: {err}",
        );
    }

    /// P1-1: the closed worker's shell refuses BOTH an explicit
    /// `background: true` arg and a trailing `&`. A detached child would
    /// outlive the attempt and dodge the deadline, so both forms fail closed
    /// (before any child is spawned).
    #[tokio::test]
    async fn closed_worker_shell_refuses_background() {
        let reg = build_fleet_worker_registry(
            &std::env::temp_dir(),
            Arc::new(NoSandbox),
            30,
            &WorkerGrant::minimal(),
            slot(),
            None,
        )
        .expect("minimal grant builds");
        let shell = reg.get("shell").expect("shell tool present");

        let explicit = shell
            .execute(&serde_json::json!({"command": "sleep 30", "background": true}))
            .await
            .unwrap();
        assert!(
            !explicit.success,
            "explicit background must be refused, got: {}",
            explicit.output
        );

        let ampersand = shell
            .execute(&serde_json::json!({"command": "sleep 30 &"}))
            .await
            .unwrap();
        assert!(
            !ampersand.success,
            "trailing-& background must be refused, got: {}",
            ampersand.output
        );
    }

    /// #1976 — a `write_paths` grant BINDS into the built registry: the
    /// worker's write_file enforces the allowlist (create allowed inside,
    /// refused outside) and the factory's violation sink records the
    /// `[denied]` audit. This is the worker-side wiring acceptance (the
    /// enforcement mechanics themselves are proved in octos-agent).
    #[tokio::test]
    async fn write_grant_binds_into_worker_registry_and_records_denials() {
        use octos_agent::tools::write_grant::{DENIED_MARKER, WriteGrantViolation};
        use std::sync::Mutex;

        let cwd = tempfile::tempdir().unwrap();
        let grant = WorkerGrant {
            write_paths: Some(vec!["exemplar.card".into()]),
            create_only: true,
            ..WorkerGrant::minimal()
        };
        let seen: Arc<Mutex<Vec<WriteGrantViolation>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let reg = build_fleet_worker_registry(
            cwd.path(),
            Arc::new(NoSandbox),
            30,
            &grant,
            slot(),
            Some(Arc::new(move |v| sink_seen.lock().unwrap().push(v))),
        )
        .expect("fenced grant builds");

        let write_file = reg.get("write_file").expect("write_file present");
        // Allowlisted create passes.
        let ok = write_file
            .execute(&serde_json::json!({"path": "exemplar.card", "content": "v1\n"}))
            .await
            .unwrap();
        assert!(ok.success, "granted create must pass: {}", ok.output);
        // Non-allowlisted write is refused + recorded.
        let denied = write_file
            .execute(&serde_json::json!({"path": "app.md", "content": "no\n"}))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied.output.contains(DENIED_MARKER), "{}", denied.output);
        assert!(!cwd.path().join("app.md").exists());
        // create_only edit_file is refused outright.
        let edit = reg.get("edit_file").expect("edit_file present");
        let edit_denied = edit
            .execute(&serde_json::json!({
                "path": "exemplar.card", "old_string": "v1", "new_string": "v2",
            }))
            .await
            .unwrap();
        assert!(!edit_denied.success, "create_only refuses edit");

        let events = seen.lock().unwrap();
        assert!(
            events.len() >= 2,
            "each refusal records to the sink, got {}",
            events.len()
        );
        assert!(events.iter().all(|v| v.detail.contains(DENIED_MARKER)));
        assert_eq!(events[0].workspace, cwd.path());
    }

    /// Discriminator: proves the audit above is not vacuous — the FULL
    /// builtin registry DOES carry the parking + fan-out tools the closed
    /// set forbids, so the exhaustive assertion genuinely rejects them.
    #[test]
    fn with_builtins_registry_would_contain_ask_user_question() {
        let reg = ToolRegistry::with_builtins(Path::new("/tmp/fleet-worker-discriminator"));
        assert!(
            reg.get("ask_user_question").is_some(),
            "with_builtins must register ask_user_question (else the audit is vacuous)",
        );
        assert!(
            reg.blocks_on_human_input("ask_user_question"),
            "ask_user_question must block on human input",
        );
        assert!(
            reg.get("spawn_agent").is_some(),
            "with_builtins must register spawn_agent",
        );
    }
}
