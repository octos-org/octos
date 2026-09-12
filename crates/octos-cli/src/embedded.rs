//! In-process native frontend using the canonical OUP runtime and dispatcher.
//! No child executable, network listener or alternate model loop is involved.

use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Serve a single frontend connection from an explicitly selected private home.
/// Dropping its pipe ends the connection; the host owns the Tokio runtime.
pub async fn serve_io<R, W>(home: &Path, reader: R, writer: W) -> eyre::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    use crate::runtime::local_oup::{LocalOupOptions, bootstrap, resolve_stored_profile};
    let data_dir = home.join(".octos");
    let config_home = home.join(".config/octos");
    let context = crate::config_context::ConfigContext {
        config_home: config_home.clone(),
        auth_home: config_home.clone(),
        data_dir: data_dir.clone(),
        is_default: false,
    };
    let host_config = crate::config::Config::load_with_context(home, &context)?;
    let profile = resolve_stored_profile(Some(octos_core::MAIN_PROFILE_ID), &data_dir)?
        .ok_or_else(|| eyre::eyre!("Configure the local LLM profile before generating a card"))?;
    let mut config = crate::profiles::config_from_profile(&profile, None, None);
    // Mobile card composition defaults to fast inference. A saved model or
    // gateway preference, or the UI's per-turn Thinking control, still wins.
    if config.model_reasoning_effort.is_none() {
        config.gateway.get_or_insert_with(Default::default)
            .reasoning_effort.get_or_insert(octos_llm::ReasoningEffort::Disabled);
    }
    crate::config::merge_host_memory_into_profile(&mut config.memory, host_config.memory.as_ref());
    config.plugins.require_signed |= host_config.plugins.require_signed;
    let state = bootstrap(LocalOupOptions {
        config,
        profile,
        data_dir,
        config_home,
        no_retry: false,
        provider: None,
        tool_profile: None,
        save_episodes: false,
    }).await?;
    crate::api::ui_protocol_transport::embedded_stdio_connection_with_io(
        state, reader, writer, Default::default(),
    ).await
}
