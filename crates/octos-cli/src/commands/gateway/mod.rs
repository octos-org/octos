//! Gateway command: run as a persistent messaging daemon.

mod account_handler;
mod adapters;
mod gateway_runtime;
#[cfg(feature = "matrix")]
mod matrix_integration;
mod message_preprocessing;
pub(crate) mod profile_factory;
pub mod prompt;
pub(crate) mod session_ui;
mod skills_handler;

use std::path::PathBuf;

use clap::Args;
use eyre::{Result, WrapErr};
use octos_core::{MAIN_PROFILE_ID, SessionKey};
use tracing::warn;

use super::Executable;

pub(crate) use prompt::build_system_prompt;

/// Run as a persistent gateway daemon.
///
/// `Serialize`/`Deserialize` back the layered startup config (see
/// [`crate::config_layer`]): non-explicit fields fall back to
/// `config.cli.gateway`.
#[derive(Debug, Args, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct GatewayCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long, conflicts_with = "profile")]
    pub config: Option<PathBuf>,

    /// Path to a profile JSON file (used by managed gateways).
    #[arg(long, conflicts_with = "config")]
    pub profile: Option<PathBuf>,

    /// Override WhatsApp bridge URL (used by managed gateways).
    #[arg(long, hide = true)]
    pub bridge_url: Option<String>,

    /// Internal: managed WeChat bridge WebSocket URL.
    #[arg(long, hide = true)]
    pub wechat_bridge_url: Option<String>,

    /// Override Feishu webhook port (used by managed gateways).
    #[arg(long, hide = true)]
    pub feishu_port: Option<u16>,

    /// Override API channel port (used by managed gateways).
    #[arg(long, hide = true)]
    pub api_port: Option<u16>,

    /// LLM provider to use (overrides config).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Custom base URL for the API endpoint (overrides config).
    #[arg(long)]
    pub base_url: Option<String>,

    /// Maximum agent iterations per message (default: 50).
    #[arg(long)]
    pub max_iterations: Option<u32>,

    /// Disable automatic retry on transient errors.
    #[arg(long)]
    pub no_retry: bool,

    /// Path to parent profile JSON (sub-accounts inherit provider config).
    #[arg(long, hide = true)]
    pub parent_profile: Option<PathBuf>,

    /// Octos home directory for ProfileStore access (used by managed gateways).
    #[arg(long, hide = true)]
    pub octos_home: Option<PathBuf>,
}

fn resolve_dispatch_profile_id(
    current_gateway_profile_id: Option<&str>,
    target_profile_id: Option<&str>,
    profile_store: Option<&crate::profiles::ProfileStore>,
) -> Result<Option<String>> {
    let Some(profile_id) = target_profile_id.filter(|value| !value.is_empty()) else {
        return Ok(current_gateway_profile_id.map(str::to_string));
    };

    if current_gateway_profile_id.is_some_and(|current| current == profile_id) {
        return Ok(Some(profile_id.to_string()));
    }

    let Some(store) = profile_store else {
        warn!(
            profile_id = %profile_id,
            "profile store unavailable; routing target profile to main profile"
        );
        return Ok(None);
    };

    match store.get(profile_id) {
        Ok(Some(_)) => Ok(Some(profile_id.to_string())),
        Ok(None) => {
            warn!(
                profile_id = %profile_id,
                "target profile not found; routing message to main profile"
            );
            Ok(None)
        }
        Err(error) => {
            warn!(
                profile_id = %profile_id,
                %error,
                "failed to load target profile; routing message to main profile"
            );
            Ok(None)
        }
    }
}

pub(crate) fn build_profiled_session_key(
    profile_id: Option<&str>,
    channel: &str,
    chat_id: &str,
    topic: &str,
) -> SessionKey {
    let effective_profile_id = profile_id.unwrap_or(MAIN_PROFILE_ID);
    SessionKey::with_profile_topic(effective_profile_id, channel, chat_id, topic)
}

/// #1506 — true when an operator-pinned route (the channel stamped
/// `profile_route_explicit` from its chat/guild map) failed to resolve to
/// its pinned target. Such a message must be REFUSED, never dispatched to
/// the fallback/default profile: an explicit mapping is a delivery contract,
/// and the legacy heuristic-route fallbacks (unknown target → main, factory
/// failure → main) would silently mis-deliver it. Heuristic routes (no
/// `profile_route_explicit` marker, e.g. Matrix mention/room routing) keep
/// their existing fallback behavior.
fn explicit_route_misresolved(
    metadata: &serde_json::Value,
    target_profile_id: Option<&str>,
    dispatch_profile_id: Option<&str>,
) -> bool {
    let explicit = metadata
        .get("profile_route_explicit")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    explicit && target_profile_id.is_some_and(|target| dispatch_profile_id != Some(target))
}


impl Executable for GatewayCommand {
    fn execute(self) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024) // 8MB stack for deep agent futures
            .build()
            .wrap_err("failed to create tokio runtime")?;
        let result = runtime.block_on(self.run_async());

        // The CLI channel uses Tokio stdin, whose blocking read can keep the
        // runtime teardown attached to the terminal after a clean shutdown.
        // `serve` uses the same pattern for long-lived background tasks.
        if result.is_ok() {
            std::process::exit(0);
        }
        result
    }
}

impl GatewayCommand {
    async fn run_async(self) -> Result<()> {
        let runtime = gateway_runtime::GatewayRuntime::init(self).await?;
        runtime.run().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1506 — the refusal boundary: only an explicit map route whose target
    /// failed to resolve is refused. Every other combination keeps its
    /// legacy behavior, including heuristic fallbacks and the no-map case.
    #[test]
    fn should_refuse_only_misresolved_explicit_profile_routes() {
        let explicit = serde_json::json!({"profile_route_explicit": true});
        let bare = serde_json::json!({});

        // Explicit map + unresolved target (unknown profile or factory
        // failure ⇒ dispatch fell back to None): refuse.
        assert!(explicit_route_misresolved(&explicit, Some("work"), None));
        // Explicit map + resolved to a DIFFERENT profile: refuse.
        assert!(explicit_route_misresolved(
            &explicit,
            Some("work"),
            Some("_main")
        ));
        // Explicit map + resolved to the target itself: dispatch.
        assert!(!explicit_route_misresolved(
            &explicit,
            Some("work"),
            Some("work")
        ));
        // No explicit marker: legacy fallback stays (heuristic route to an
        // unknown profile still falls back to main).
        assert!(!explicit_route_misresolved(&bare, Some("work"), None));
        assert!(!explicit_route_misresolved(
            &bare,
            Some("work"),
            Some("_main")
        ));
        // No target at all (no map hit): nothing to refuse.
        assert!(!explicit_route_misresolved(&explicit, None, None));
        assert!(!explicit_route_misresolved(&bare, None, None));
        // A false marker is not explicit.
        let not_explicit = serde_json::json!({"profile_route_explicit": false});
        assert!(!explicit_route_misresolved(
            &not_explicit,
            Some("work"),
            None
        ));
    }
}
