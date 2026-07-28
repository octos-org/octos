//! Module 1 — the closed, replay-safe worker tool registry (**the crux**).
//!
//! A fleet task-worker is *provably* non-interactive: it may hold only the
//! seven native tools that are safe to run and re-run headless (read/write/
//! edit files, glob/grep search, list dirs, and a fail-closed shell). It
//! must NOT be able to park (`ask_user_question`/`request_user_input`), fan
//! out (`spawn*`/`delegate*`/`peer_*`), reach the network
//! (`web_*`/`http`/`browser`/deep-crawl), message a channel (`message`/
//! `send_*`), or mutate durable memory (`recall_memory`/`save_memory`/…).
//!
//! [`build_fleet_worker_registry`] starts from an EMPTY [`ToolRegistry`]
//! (never [`ToolRegistry::with_builtins`], which registers ~35 tools
//! including the parking + fan-out set), registers exactly the allow-set,
//! and then hard-removes anything else with [`ToolRegistry::apply_policy`]
//! as a belt-and-suspenders lock on the invariant. The exhaustive audit
//! test asserts `tool_names() == ALLOWED`, so any future dynamic tool
//! (MCP/plugin/skill) sneaking in fails the build.

use std::path::Path;
use std::sync::Arc;

use octos_agent::policy::{ApprovalPolicy, EffectivePermissions};
use octos_agent::sandbox::Sandbox;
use octos_agent::tools::policy::ToolPolicy;
use octos_agent::tools::{
    EditFileTool, GlobTool, GrepTool, ListDirTool, ReadFileTool, ShellTool, ToolRegistry,
    WriteFileTool,
};

/// The EXACT set of native tool names a closed fleet task-worker may hold.
/// Every one is replay-safe (idempotent-enough to re-run after a crash) and
/// none blocks on human input. The audit test asserts the built registry's
/// `tool_names()` equals this set — no more, no less.
pub const ALLOWED: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob",
    "grep",
    "list_dir",
    "shell",
];

/// Build the closed, replay-safe tool registry for a fleet task-worker
/// rooted at `cwd`, with `sandbox` backing the shell tool and each shell
/// command's effective timeout CAPPED at `max_shell_timeout_secs` (the attempt
/// deadline, in whole seconds) so no single foreground command outlives it.
///
/// The closed tool set is a DENYLIST (it removes parking/fan-out/network
/// *tools*), NOT a network or process boundary. The surviving `shell` can
/// still reach the network and can still detach a child via arbitrary
/// shell-internal backgrounding that string inspection cannot catch (e.g.
/// `sleep 600 & true`). Both are bounded by the **sandbox**: production MUST
/// supply a network-isolated sandbox whose process-group/container teardown
/// reaps detached children. Passing a no-op sandbox here is an operator error
/// (flagged with a `tracing::warn!`), analogous to `--danger-full-access`.
pub fn build_fleet_worker_registry(
    cwd: &Path,
    sandbox: Arc<dyn Sandbox>,
    max_shell_timeout_secs: u64,
) -> ToolRegistry {
    // P1-3-enforce (document, don't type-enforce): the API cannot police
    // sandbox quality, but a no-op sandbox leaves the shell's network reach and
    // detached children unbounded — surface it so it can't pass silently.
    if sandbox.is_noop() {
        tracing::warn!(
            "fleet worker: building a closed registry with a NO-OP sandbox — the \
             shell's network reach and detached children are UNBOUNDED; production \
             must supply a network-isolated sandbox",
        );
    }

    // Workspace read/write, but approvals FAIL CLOSED at the tool boundary:
    // a shell command that the SafePolicy would ask about is denied outright
    // (a closed worker has no human to ask). `SafePolicy` is preserved — we
    // do NOT widen to AllowAll — so dangerous commands stay blocked.
    let perms = EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never);
    let scope = perms.filesystem_scope; // Workspace
    let access = perms.file_access; // ReadWrite

    let mut r = ToolRegistry::new();
    // Mirror `with_builtins`: record the workspace root so the agent's
    // session-scope fallback and any project-root machinery resolve against
    // the same cwd the tools are bound to.
    r.set_workspace_root(cwd.to_path_buf());
    r.register(
        ShellTool::new(cwd)
            .with_shared_sandbox(sandbox)
            .with_policy(perms.shell_command_policy())
            .with_approval_policy(ApprovalPolicy::Never)
            // Refuse the string-detectable detach vectors (`background: true`
            // and a trailing `&`) as defense-in-depth — the sandbox is the real
            // boundary for detached children (see the fn doc).
            .with_background_allowed(false)
            // Hard per-command CEILING at the attempt deadline: combined with
            // `background_allowed(false)`, no single foreground command outlives
            // the deadline even if the LLM requests a larger `timeout_secs`.
            .with_max_timeout_secs(max_shell_timeout_secs),
    );
    r.register(ReadFileTool::new(cwd).with_filesystem_scope(scope));
    r.register(
        WriteFileTool::new(cwd)
            .with_filesystem_scope(scope)
            .with_file_access(access),
    );
    r.register(
        EditFileTool::new(cwd)
            .with_filesystem_scope(scope)
            .with_file_access(access),
    );
    r.register(GlobTool::new(cwd).with_filesystem_scope(scope));
    r.register(GrepTool::new(cwd));
    r.register(ListDirTool::new(cwd).with_filesystem_scope(scope));

    // Belt-and-suspenders: hard-remove anything not in the allow-set. A no-op
    // today (we registered exactly the allow-set and nothing auto-swaps,
    // because no tool is named "spawn"), but it locks the invariant so a
    // future edit that registers an extra tool cannot silently widen the
    // worker's reach — `apply_policy` -> `retain` is a real removal.
    r.apply_policy(&ToolPolicy {
        allow: ALLOWED.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    });
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_agent::sandbox::NoSandbox;
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::Arc;

    /// Tools that must NEVER appear in a closed worker registry. This is a
    /// supplementary explicit denylist; the exhaustive `tool_names() ==
    /// ALLOWED` assertion below is the real guard (it also catches names not
    /// listed here). Covers the parking, fan-out, peer, channel, network,
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
        // network / external
        "browser",
        "web_search",
        "web_fetch",
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

    fn allowed_sorted() -> Vec<String> {
        let mut v: Vec<String> = ALLOWED.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn closed_worker_registry_has_only_replay_safe_tools() {
        let reg = build_fleet_worker_registry(
            Path::new("/tmp/fleet-worker-audit"),
            Arc::new(NoSandbox),
            30,
        );

        // (2) EXHAUSTIVE: the registry contains EXACTLY the allow-set — no
        // more, no less. This is the boundary that catches any future
        // dynamic (MCP/plugin/skill) tool sneaking in.
        let mut names = reg.tool_names();
        names.sort();
        assert_eq!(
            names,
            allowed_sorted(),
            "closed worker registry must contain EXACTLY the replay-safe tools",
        );

        let spec_names: HashSet<String> = reg.specs().into_iter().map(|s| s.name).collect();

        // (1) every FORBIDDEN name is un-gettable AND absent from specs().
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

        // (3) NOTHING in the registry blocks on human input.
        for name in reg.tool_names() {
            assert!(
                !reg.blocks_on_human_input(&name),
                "tool {name} blocks on human input — a closed worker must never park",
            );
        }

        // Sanity: every allowed tool is actually present + LLM-visible.
        for name in ALLOWED {
            assert!(
                reg.get(name).is_some(),
                "allowed tool {name} missing from closed registry",
            );
            assert!(
                spec_names.contains(*name),
                "allowed tool {name} not exposed in specs()",
            );
        }
    }

    /// P1-1: the closed worker's shell refuses BOTH an explicit
    /// `background: true` arg and a trailing `&`. A detached child would
    /// outlive the attempt and dodge the deadline, so both forms fail closed
    /// (before any child is spawned).
    #[tokio::test]
    async fn closed_worker_shell_refuses_background() {
        let reg = build_fleet_worker_registry(&std::env::temp_dir(), Arc::new(NoSandbox), 30);
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
