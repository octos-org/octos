//! octos-fleet-worker — the closed, non-interactive fleet task worker.
//!
//! This crate is the executor half of the fleet kernel (see `octos-fleet`
//! for the durable store). It provides:
//!
//! - [`build_fleet_worker_registry`] — the **closed** replay-safe tool
//!   registry (the crux: a worker that provably cannot park or fan out);
//! - [`run_attempt`] — the per-attempt executor that runs one plan task
//!   under a hard deadline, gates it on acceptance criteria, and records
//!   the real outcome to the [`octos_fleet::FleetKernelStore`];
//! - [`FleetWorkerPool`] — the bounded pool that launches a ready task and
//!   runs its attempt under global + per-fleet concurrency permits.
//!
//! It deliberately does NOT own the pool lifecycle, consume the outbox,
//! wake the keeper, or expose any LLM tool — those land in later PRs.
//!
//! # The sandbox is the boundary (not the tool list)
//!
//! The closed registry is a *denylist* of tool names — it removes parking,
//! fan-out, and network *tools*. It is NOT a network or process boundary: the
//! surviving `shell` can still reach the network (`curl`, `wget`, a non-force
//! `git push`) and can still detach a child via arbitrary shell-internal
//! backgrounding (`sleep 600 & true`, `sh -c "cmd &"`) that string inspection
//! cannot catch. Both are bounded only by the **sandbox** and its process-group
//! /container teardown, which is why [`AgentFactory::new`] requires an explicit
//! sandbox factory (no silent no-op default) and production MUST supply a
//! network-isolated sandbox — an operator requirement the API cannot enforce at
//! the type level (a no-op sandbox is flagged with a `tracing::warn!`).
//! [`AgentFactory::for_testing`] is the only no-op path and is gated behind
//! `cfg(test)`.
//!
//! # Deadline enforcement
//!
//! [`run_attempt`] wraps the agent run in a hard [`tokio::time::timeout`],
//! [`AgentFactory`] clamps the agent's per-tool timeouts to the deadline, the
//! worker shell also carries a hard per-command CEILING at the deadline (so a
//! foreground command cannot outlive it even if the LLM requests a larger
//! `timeout_secs`), the string-detectable background vectors are refused, and
//! the acceptance phase is itself bounded by the remaining deadline. The
//! residual: a child detached by arbitrary shell-internal backgrounding, or a
//! tool task the agent loop detaches on its own timeout, is reaped only by the
//! sandbox's process-group teardown — not by this crate.
#![deny(unsafe_code)]

mod closed_registry;
mod escalate;
mod pool;
mod worker;

#[cfg(test)]
mod testutil;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use octos_agent::sandbox::Sandbox;
use octos_agent::{Agent, AgentConfig, ToolRegistry};
use octos_core::AgentId;
use octos_fleet::WorkerGrant;
use octos_llm::LlmProvider;
use octos_memory::EpisodeStore;

pub use closed_registry::{ALLOWED, build_fleet_worker_registry};
pub use escalate::{EscalateTool, EscalationSlot};
pub use pool::{Dispatched, FleetWorkerPool, PoolConfig};
pub use worker::{AttemptOutcome, WorktreeContext, run_attempt};

/// The per-attempt sandbox scope beyond the isolating backend: what a single
/// attempt's [`WorkerGrant`] widens on top of the base (network-off,
/// cwd-only-writable) sandbox.
///
/// - `allow_network` — from the grant's network lane: `None`/`Hosts` → `false`
///   (no raw egress; `Hosts` is enforced by the granted web tools), `Full` →
///   `true` (raw egress for git/npm/etc.).
/// - `repo_git_dir` — from the grant's FS lane: `FsGrant::Host` (a worktree
///   worker) → `Some(<repo>/.git)`, so the sandbox rw-binds exactly that repo
///   `.git` common dir (the ONLY writable path outside the checkout cwd its
///   `git commit` needs — objects/refs/worktree-admin); else `None` (cwd-only).
///   A TARGETED bind, NOT full-`/`, so no host AF_UNIX socket is ever exposed.
///
/// Both default (network-off, `repo_git_dir` `None`) = today's isolated,
/// cwd-only-writable worker. The factory folds this onto the base
/// `SandboxConfig`; the isolating BACKEND (bwrap/macos/docker) is owned by the
/// factory, only the scope is per-attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxGrant {
    /// Whether the shell may reach the network (raw egress).
    pub allow_network: bool,
    /// The repo `.git` common dir to rw-bind beyond the cwd (operator
    /// `FsGrant::Host` worktree worker), or `None` for a cwd-only worker.
    pub repo_git_dir: Option<PathBuf>,
}

/// A per-attempt sandbox factory: given a working directory AND the
/// [`SandboxGrant`] scope that attempt needs, produce the [`Sandbox`] backing
/// its shell tool. The factory folds the grant onto its base `SandboxConfig`
/// (`allow_network` + `repo_git_write`); `SandboxGrant::default()` is the base
/// (network-off, cwd-only-writable) worker.
pub type SandboxFactory = Arc<dyn Fn(&Path, SandboxGrant) -> Arc<dyn Sandbox> + Send + Sync>;

/// Default agent-loop iteration ceiling for a fleet task-worker.
pub const DEFAULT_MAX_ITERATIONS: u32 = 50;

/// Builds fresh, closed-registry [`Agent`]s (and matching validator
/// registries) for fleet task attempts.
///
/// Every agent is minted with the same shared LLM provider and episodic
/// memory, a per-cwd sandbox from the EXPLICIT `sandbox_factory` (the shell's
/// network reach is bounded by the sandbox, not the closed tool list — see the
/// crate docs), the closed replay-safe tool registry, and an [`AgentConfig`]
/// with `save_episodes: false` (a fleet worker's episodic write-back is owned
/// by the keeper, not the throwaway attempt) whose per-tool timeouts are
/// clamped to the attempt deadline.
#[derive(Clone)]
pub struct AgentFactory {
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    sandbox_factory: SandboxFactory,
    max_iterations: u32,
    max_tokens: Option<u32>,
}

impl AgentFactory {
    /// A factory over a shared provider + episodic store and an EXPLICIT
    /// per-cwd sandbox factory.
    ///
    /// The sandbox is mandatory by design: the closed tool set is a denylist,
    /// not a boundary, so the shell's reach (`curl`/`wget`/`git push`) is
    /// bounded only by the sandbox. Production MUST pass a network-isolated
    /// sandbox (bwrap/docker/macos). There is deliberately no silent no-op
    /// default — [`AgentFactory::for_testing`] is the only [`NoSandbox`] path.
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        sandbox_factory: SandboxFactory,
    ) -> Self {
        Self {
            llm,
            memory,
            sandbox_factory,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_tokens: None,
        }
    }

    /// TEST-ONLY constructor whose shell tool runs with NO sandbox
    /// ([`NoSandbox`]). This is the ONLY no-op-sandbox path in the crate and is
    /// gated behind `cfg(test)`, so no production build can ship an
    /// un-sandboxed fleet worker. Everywhere else use [`AgentFactory::new`]
    /// with a real sandbox factory.
    #[cfg(test)]
    pub fn for_testing(llm: Arc<dyn LlmProvider>, memory: Arc<EpisodeStore>) -> Self {
        use octos_agent::sandbox::NoSandbox;
        Self::new(
            llm,
            memory,
            Arc::new(|_, _| Arc::new(NoSandbox) as Arc<dyn Sandbox>),
        )
    }

    /// Override the agent-loop iteration ceiling.
    pub fn with_max_iterations(mut self, max_iterations: u32) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Override the per-attempt total-token ceiling (`None` = unlimited).
    pub fn with_max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// The per-cwd [`Sandbox`] backing this factory's shell tool. `run_attempt`
    /// calls this ONCE per attempt and threads the single instance into both
    /// the agent registry and the acceptance validators (see
    /// [`AgentFactory::build_agent`] / [`AgentFactory::build_registry_with`]),
    /// so a non-idempotent factory can never sandbox the agent while handing
    /// the validators a different (weaker) sandbox.
    ///
    /// `grant` is the per-attempt [`SandboxGrant`] scope (network + full-FS
    /// write) folded onto the factory's base sandbox. Pass
    /// `SandboxGrant::default()` for the base (network-off, cwd-only) worker.
    pub fn sandbox_for(&self, cwd: &Path, grant: SandboxGrant) -> Arc<dyn Sandbox> {
        (self.sandbox_factory)(cwd, grant)
    }

    /// Build the granted replay-safe tool registry for `cwd` FROM `grant`, with
    /// an EXPLICIT, caller-owned `sandbox` instance and a per-command shell
    /// timeout ceiling of `max_shell_timeout_secs`. Threading the instance
    /// (rather than re-invoking the factory) is what lets the agent and the
    /// acceptance-gate [`octos_agent::ValidatorRunner`] share one sandbox.
    ///
    /// Returns `Err` for an incoherent grant (unknown tool / web tool without a
    /// network grant) — validated at parse too, so this is defense-in-depth.
    ///
    /// `escalation` is the shared slot the always-on `escalate` valve writes
    /// into; the agent path threads the attempt's real slot (which
    /// [`run_attempt`] reads after the turn), while the acceptance-validator
    /// path passes a throwaway (validators never call tools).
    pub fn build_registry_with(
        &self,
        cwd: &Path,
        sandbox: Arc<dyn Sandbox>,
        max_shell_timeout_secs: u64,
        grant: &WorkerGrant,
        escalation: EscalationSlot,
    ) -> Result<ToolRegistry> {
        build_fleet_worker_registry(cwd, sandbox, max_shell_timeout_secs, grant, escalation)
    }

    /// The [`AgentConfig`] for an attempt bounded by `deadline`. The per-tool
    /// timeouts (`tool_timeout_secs` and the interactive default) are CLAMPED
    /// to the deadline so no single tool call can outlive the fleet deadline by
    /// more than its own now-bounded timeout. See the crate docs for the
    /// residual soft-deadline caveat.
    pub(crate) fn agent_config(&self, deadline: Duration) -> AgentConfig {
        let deadline_secs = deadline.as_secs().max(1);
        let base = AgentConfig::default();
        AgentConfig {
            max_iterations: self.max_iterations,
            max_tokens: self.max_tokens,
            save_episodes: false,
            tool_timeout_secs: base.tool_timeout_secs.min(deadline_secs),
            default_interactive_tool_timeout_secs: base
                .default_interactive_tool_timeout_secs
                .min(deadline_secs),
            ..base
        }
    }

    /// Build a fresh, granted-registry [`Agent`] rooted at `cwd` under the
    /// shared `sandbox`, from `grant`, bounded by `deadline` (which clamps the
    /// agent's per-tool timeouts AND caps every shell command at the deadline).
    ///
    /// Returns `Err` for an incoherent grant (see [`AgentFactory::build_registry_with`]).
    pub fn build_agent(
        &self,
        cwd: &Path,
        sandbox: Arc<dyn Sandbox>,
        deadline: Duration,
        grant: &WorkerGrant,
        escalation: EscalationSlot,
    ) -> Result<Agent> {
        let deadline_secs = deadline.as_secs().max(1);
        Ok(Agent::new(
            AgentId::new("fleet-worker"),
            self.llm.clone(),
            self.build_registry_with(cwd, sandbox, deadline_secs, grant, escalation)?,
            self.memory.clone(),
        )
        .with_config(self.agent_config(deadline)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{SuccessProvider, fresh_memory};
    use octos_agent::sandbox::NoSandbox;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::process::Command;

    /// A non-no-op marker sandbox: inherits the default `is_noop() == false`,
    /// so a factory that threads it proves the injected sandbox reaches the
    /// shell path rather than being replaced by a silent `NoSandbox`.
    struct MarkerSandbox;
    impl Sandbox for MarkerSandbox {
        fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
            NoSandbox.wrap_command(shell_command, cwd)
        }
    }

    /// P1-3: the sandbox factory passed to `new` is actually threaded to the
    /// shell path — there is no silent `NoSandbox` default overriding it — and
    /// `for_testing` is the ONLY no-op-sandbox path.
    #[tokio::test]
    async fn sandbox_factory_is_threaded_not_defaulted() {
        let (_m1, memory) = fresh_memory().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let factory = AgentFactory::new(
            Arc::new(SuccessProvider),
            memory,
            Arc::new(move |_, _| {
                seen.fetch_add(1, Ordering::SeqCst);
                Arc::new(MarkerSandbox) as Arc<dyn Sandbox>
            }),
        );

        let sb = factory.sandbox_for(Path::new("/tmp/fleet-x"), SandboxGrant::default());
        assert!(
            !sb.is_noop(),
            "the injected sandbox must reach the shell, not a NoSandbox default",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "sandbox_for must invoke the injected factory",
        );

        // build_registry_with threads the caller-owned instance WITHOUT
        // re-invoking the factory — the shared-instance contract (P1-3-fix).
        let _reg = factory
            .build_registry_with(
                Path::new("/tmp/fleet-x"),
                sb.clone(),
                30,
                &WorkerGrant::minimal(),
                Arc::new(std::sync::Mutex::new(None)),
            )
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "build_registry_with must NOT re-invoke the sandbox factory",
        );

        let (_m2, memory2) = fresh_memory().await;
        let test_factory = AgentFactory::for_testing(Arc::new(SuccessProvider), memory2);
        assert!(
            test_factory
                .sandbox_for(Path::new("/tmp/fleet-x"), SandboxGrant::default())
                .is_noop(),
            "for_testing must be the NoSandbox path",
        );
    }

    /// P1-2: the agent config clamps per-tool timeouts to the attempt deadline,
    /// so no single tool call is configured to outlive it.
    #[tokio::test]
    async fn tool_timeouts_are_clamped_to_the_deadline() {
        let (_m, memory) = fresh_memory().await;
        let factory = AgentFactory::for_testing(Arc::new(SuccessProvider), memory);

        let deadline = Duration::from_secs(5);
        let cfg = factory.agent_config(deadline);
        assert!(
            cfg.tool_timeout_secs <= deadline.as_secs(),
            "tool_timeout_secs {} must be <= deadline {}s",
            cfg.tool_timeout_secs,
            deadline.as_secs(),
        );
        assert!(
            cfg.default_interactive_tool_timeout_secs <= deadline.as_secs(),
            "interactive tool timeout {} must be <= deadline {}s",
            cfg.default_interactive_tool_timeout_secs,
            deadline.as_secs(),
        );
        assert!(!cfg.save_episodes, "fleet attempts must not save episodes");

        // A deadline longer than the stock ceilings leaves them unchanged.
        let long = Duration::from_secs(100_000);
        let base = AgentConfig::default();
        assert_eq!(
            factory.agent_config(long).tool_timeout_secs,
            base.tool_timeout_secs,
            "a long deadline must not inflate the stock tool timeout",
        );
    }
}
