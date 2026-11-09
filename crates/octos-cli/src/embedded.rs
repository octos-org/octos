//! In-process native frontend using the canonical OUP runtime and dispatcher.
//! No child executable, network listener or alternate model loop is involved.

use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Serve a single frontend connection from an explicitly selected private home.
/// Dropping its pipe ends the connection; the host owns the Tokio runtime.
///
/// # Runtime requirement: 8 MiB worker stacks
///
/// The AppUI dispatcher's poll chain is deeper than Tokio's default 2 MiB
/// worker stack. Every in-repo entry point (chat, ACP, gateway, MCP) builds
/// its runtime with `thread_stack_size(8 * 1024 * 1024)`, and the host's
/// runtime must do the same — on a default-configured runtime (for example a
/// plain `#[tokio::main]`) the first `session/open` overflows the worker
/// stack and aborts the process instead of returning an error:
///
/// ```no_run
/// # async fn serve(home: &std::path::Path, io: tokio::io::DuplexStream) -> eyre::Result<()> {
/// # let (reader, writer) = tokio::io::split(io);
/// # octos_cli::embedded::serve_io(home, reader, writer).await
/// # }
/// let runtime = tokio::runtime::Builder::new_multi_thread()
///     .enable_all()
///     .thread_stack_size(8 * 1024 * 1024)
///     .build()?;
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// Spawn `serve_io` onto such a runtime (`runtime.spawn(...)`) rather than
/// `block_on`-ing it from a thread with a smaller stack: `block_on` polls the
/// future on the calling thread.
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
        config
            .gateway
            .get_or_insert_with(Default::default)
            .reasoning_effort
            .get_or_insert(octos_llm::ReasoningEffort::Disabled);
    }
    crate::config::merge_host_memory_into_profile(&mut config.memory, host_config.memory.as_ref());
    config.plugins.require_signed |= host_config.plugins.require_signed;
    // Build the provider here rather than letting bootstrap call
    // `create_provider`, which prints a colored `Model: <id>` line to stderr —
    // noise for a host that owns its process's stderr.
    let provider_name = crate::runtime::profile::configured_provider_name(&config)
        .ok_or_else(|| eyre::eyre!("profile '{}' has no LLM provider configured", profile.id))?;
    let provider = crate::commands::chat::create_provider_with_api_type(
        &provider_name,
        &config,
        config.model.clone(),
        config.base_url.clone(),
        config.api_type.as_deref(),
    )?;
    let state = bootstrap(LocalOupOptions {
        config,
        profile,
        data_dir,
        config_home,
        no_retry: false,
        provider: Some(provider),
        tool_profile: None,
        save_episodes: false,
    })
    .await?;
    crate::api::ui_protocol_transport::embedded_stdio_connection_with_io(
        state,
        reader,
        writer,
        Default::default(),
    )
    .await
}
