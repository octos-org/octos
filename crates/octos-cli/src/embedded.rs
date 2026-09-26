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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Store the `_main` profile whose primary model is a keyless local
    /// OpenAI-compatible endpoint at `base_url`. `_main` is reserved, so it
    /// bypasses `ProfileStore::save`'s id validation the way a host writes it.
    fn store_main_profile(home: &Path, base_url: &str) {
        let store = crate::profiles::ProfileStore::open_unified(&home.join(".octos"))
            .expect("profile store");
        let profile = json!({
            "id": octos_core::MAIN_PROFILE_ID,
            "name": "Main",
            "enabled": true,
            "config": {
                "llm": {
                    "primary": {
                        "family_id": "local",
                        "model_id": "fixture-model",
                        "route": { "base_url": base_url },
                    },
                },
            },
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        });
        let path = store.profile_path(octos_core::MAIN_PROFILE_ID);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, profile.to_string()).expect("write profile");
    }

    /// Happy path the way a host embeds it: a stored profile, `serve_io`
    /// spawned onto a runtime with the documented 8 MiB worker stacks, a
    /// `session/open` answered over the pipe, and a clean `Ok(())` once the
    /// host drops its end.
    #[test]
    fn serve_io_opens_a_session_on_an_8_mib_stack_runtime() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "object": "list",
                    "data": [{ "id": "fixture-model", "object": "model" }],
                })))
                .mount(&server)
                .await;
            let home = tempfile::tempdir().unwrap();
            store_main_profile(home.path(), &format!("{}/v1", server.uri()));
            let workspace = tempfile::tempdir().unwrap();

            let (client, server_io) = tokio::io::duplex(1024 * 1024);
            let (reader, writer) = tokio::io::split(server_io);
            let home_path = home.path().to_owned();
            let serving = tokio::spawn(async move { serve_io(&home_path, reader, writer).await });

            let (client_reader, mut client_writer) = tokio::io::split(client);
            let session_id = octos_core::SessionKey::with_profile(
                octos_core::MAIN_PROFILE_ID,
                "embedded",
                "happy-path",
            );
            let open = json!({
                "jsonrpc": "2.0",
                "id": "open",
                "method": octos_core::ui_protocol::methods::SESSION_OPEN,
                "params": {
                    "session_id": session_id,
                    "profile_id": octos_core::MAIN_PROFILE_ID,
                    "cwd": workspace.path().to_string_lossy(),
                },
            });
            client_writer
                .write_all(format!("{open}\n").as_bytes())
                .await
                .unwrap();

            let mut lines = BufReader::new(client_reader).lines();
            let reply = tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    let line = lines
                        .next_line()
                        .await
                        .expect("read frame")
                        .expect("connection stays open until the reply");
                    let frame: Value = serde_json::from_str(&line).expect("json frame");
                    if frame["id"] == "open" {
                        break frame;
                    }
                }
            })
            .await
            .expect("session/open replies");
            assert!(reply.get("error").is_none(), "session/open failed: {reply}");
            assert_eq!(
                reply["result"]["opened"]["session_id"],
                json!(session_id),
                "{reply}"
            );

            // Close the host's input but keep draining output until the
            // server closes it: frames still in flight (e.g. session
            // notifications) must not hit a dropped reader.
            // A split half does not close the duplex on drop; shut it down.
            client_writer.shutdown().await.unwrap();
            drop(client_writer);
            let drain =
                tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
            tokio::time::timeout(Duration::from_secs(30), serving)
                .await
                .expect("serve_io ends once the host drops the pipe")
                .expect("serve_io task does not panic")
                .expect("serve_io returns Ok on a clean close");
            drain.await.expect("drain task does not panic");
        });
    }

    #[tokio::test]
    async fn serve_io_requires_a_stored_main_profile() {
        let home = tempfile::tempdir().unwrap();
        let (_client, server) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(server);
        let err = serve_io(home.path(), reader, writer).await.unwrap_err();
        assert!(
            err.to_string().contains("Configure the local LLM profile"),
            "{err}"
        );
    }
}
