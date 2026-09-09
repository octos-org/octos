//! `octos acp`: run octos as an [Agent Client Protocol][acp] (ACP) agent over
//! stdin/stdout so ACP clients (Zed, and other editors/CLIs that speak ACP) can
//! drive the octos agent loop.
//!
//! [acp]: https://agentclientprotocol.com/
//!
//! ## Shape
//!
//! The official `agent-client-protocol` crate (v1.2.0) does **not** expose a
//! single "implement these methods" trait. Instead you build an
//! [`agent_client_protocol::Agent`] role via its builder, registering one
//! `on_receive_request` / `on_receive_notification` handler per ACP message
//! type, then hand the builder a stdio transport and let its runtime drive the
//! JSON-RPC framing:
//!
//! ```ignore
//! Agent.builder()
//!     .on_receive_request(handle_initialize, on_receive_request!())
//!     .on_receive_request(handle_new_session, on_receive_request!())
//!     .on_receive_request(handle_prompt, on_receive_request!())
//!     .on_receive_notification(handle_cancel, on_receive_notification!())
//!     .connect_to(Stdio::new())
//!     .await
//! ```
//!
//! Dispatch is by the request's Rust type: registering a handler for
//! [`InitializeRequest`] wires the `"initialize"` method, `NewSessionRequest`
//! wires `"session/new"`, `PromptRequest` wires `"session/prompt"`,
//! `CancelNotification` wires the `"session/cancel"` notification, and so on.
//!
//! Each handler receives a `Responder<Resp>` (call `.respond(resp)` to reply)
//! and a `ConnectionTo<Client>` (`cx`) on which we push streaming
//! [`SessionNotification`]s (`session/update`) back to the client.
//!
//! ## Bridge
//!
//! ACP is a transport adapter over the OUP dispatcher. Session persistence,
//! execution, compaction, tools and cancellation belong to the shared runtime.
//! Streaming projects typed OUP envelopes into ACP updates; tool approvals use
//! ACP request_permission. There is no ACP-owned model loop or history.

#[cfg(feature = "api")]
use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(feature = "api")]
use std::sync::Arc;
#[cfg(feature = "api")]
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Args;
use eyre::{Result, WrapErr};

#[cfg(any(feature = "api", test))]
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, InitializeRequest, InitializeResponse, PromptCapabilities,
    SessionId,
};
#[cfg(feature = "api")]
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentChunk, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, SessionNotification, SessionUpdate,
    StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
#[cfg(feature = "api")]
use agent_client_protocol::{
    Agent as AcpAgentRole, Client, ConnectionTo, Error as AcpError, Stdio, on_receive_notification,
    on_receive_request,
};

#[cfg(feature = "api")]
use octos_agent::Agent;
#[cfg(feature = "api")]
use octos_llm::LlmProvider;
#[cfg(feature = "api")]
use tokio::sync::Mutex;

#[cfg(feature = "api")]
use octos_bus::session::SessionManager;
#[cfg(feature = "api")]
use octos_core::SessionKey;

use super::Executable;
#[cfg(feature = "api")]
mod oup;
#[cfg(feature = "api")]
use crate::config::Config;

/// Default for [`AcpCommand::max_iterations`]. Shared by the clap default and
/// the `Default` impl so an embedder building the command by hand gets the same
/// budget the CLI does.
pub const DEFAULT_MAX_ITERATIONS: u32 = 0;

/// Run octos as an ACP (Agent Client Protocol) agent over stdin/stdout.
///
/// ACP clients (Zed and other editors) spawn this process and drive it via
/// JSON-RPC on stdio. Provider/model/profile flags mirror `octos chat` so the
/// ACP agent resolves an LLM the same way.
#[derive(Debug, Args)]
pub struct AcpCommand {
    /// Working directory the agent's tools are rooted at (defaults to the
    /// current directory). Note: ACP clients also send a `cwd` with
    /// `session/new`; that per-session value takes precedence when present.
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes/memory (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// LLM provider to use (overrides config).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Custom base URL for the API endpoint (overrides config).
    #[arg(long)]
    pub base_url: Option<String>,

    /// Maximum LLM-loop iterations per prompt turn. 0 means unlimited.
    #[arg(long, default_value_t = DEFAULT_MAX_ITERATIONS)]
    pub max_iterations: u32,

    /// Runtime profile to apply at startup (parity with `octos chat`).
    /// Accepts a built-in name (`coding`, `coding-full`, `swarm`), a
    /// user-dir id under `~/.octos/profiles/<id>/`, or a path. Defaults to
    /// `coding`, the lean core-coding tool surface; use `coding-full` for
    /// the unfiltered pre-lean tool set.
    #[arg(long)]
    pub profile: Option<String>,
}

impl Default for AcpCommand {
    /// Keep the interactive unlimited sentinel identical to clap's default.
    fn default() -> Self {
        Self {
            cwd: None,
            data_dir: None,
            config: None,
            provider: None,
            model: None,
            base_url: None,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            profile: None,
        }
    }
}

impl Executable for AcpCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024) // deep agent futures need a big stack
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

/// Supplies the shared OUP runtime to ACP and in-process embedders.
///
/// Production uses [`ConfigAgentFactory`]; tests inject a provider without
/// replacing the dispatcher. Requires the default `api` feature.
#[cfg(feature = "api")]
#[async_trait::async_trait]
pub trait SessionAgentFactory: Send + Sync {
    /// The cwd to fall back to when the client sends an empty `session/new` cwd.
    fn default_cwd(&self) -> &std::path::Path;

    /// Build a runnable agent rooted at `cwd`, returning it plus the shared
    /// shutdown flag wired into it (flipped on `session/cancel`).
    async fn build(&self, cwd: PathBuf) -> Result<(Arc<Agent>, Arc<AtomicBool>)> {
        let state = self.oup_state().await?;
        let profile_id = self.session_profile_id();
        let profile = state
            .profiles
            .get(&profile_id)
            .ok_or_else(|| eyre::eyre!("OUP profile runtime unavailable: {profile_id}"))?;
        let key = SessionKey::with_profile(&profile_id, "acp", &uuid::Uuid::now_v7().to_string());
        let runtime = state
            .session_cache
            .get_or_init(profile, key, Some(cwd))
            .await?;
        Ok((runtime.agent.clone(), runtime.agent.shutdown_signal()))
    }

    /// Canonical runtime used by the ACP protocol adapter. `build` remains
    /// available to embedders that explicitly request a bare Agent; it is not
    /// the ACP transport's execution path.
    async fn oup_state(&self) -> Result<Arc<crate::api::AppState>> {
        eyre::bail!("this embedding factory does not provide an OUP runtime")
    }

    /// Where ACP conversations are persisted, if anywhere. `None` means sessions
    /// will not survive a restart — the test factory, or a store that failed to open.
    fn session_store(&self) -> Option<Arc<Mutex<SessionManager>>> {
        None
    }

    /// Profile dimension for this factory's session keys. Sessions elsewhere are
    /// profile-scoped; an unscoped ACP key would collide across profiles running
    /// the same session id and would not isolate them.
    fn session_profile_id(&self) -> String {
        octos_core::MAIN_PROFILE_ID.to_string()
    }
}

/// Lazily initializes the same profile/session runtime used by OUP.
/// Initialization errors are reported by session/new, not the ACP handshake.
#[cfg(feature = "api")]
struct ConfigAgentFactory {
    config: Config,
    provider_name: String,
    model: Option<String>,
    base_url: Option<String>,
    data_dir: PathBuf,
    default_cwd: PathBuf,
    max_iterations: u32,
    profile: Option<String>,
    #[cfg(feature = "api")]
    oup_state: tokio::sync::OnceCell<Arc<crate::api::AppState>>,
}

#[cfg(feature = "api")]
#[async_trait::async_trait]
impl SessionAgentFactory for ConfigAgentFactory {
    #[cfg(feature = "api")]
    async fn oup_state(&self) -> Result<Arc<crate::api::AppState>> {
        self.oup_state
            .get_or_try_init(|| async {
                use crate::runtime::local_oup::{
                    LocalOupOptions, bootstrap, local_profile, resolve_stored_profile,
                };
                let mut config = self.config.clone();
                config.provider = Some(self.provider_name.clone());
                config.model = self.model.clone();
                config.base_url = self.base_url.clone();
                config.max_iterations = Some(self.max_iterations);
                let stored = resolve_stored_profile(self.profile.as_deref(), &self.data_dir)?;
                let tool_profile = match super::chat::resolve_profile(&self.profile) {
                    Ok((profile, _)) => profile,
                    Err(_) if stored.is_some() => super::chat::resolve_profile(&None)?.0,
                    Err(error) => return Err(error),
                };
                let mut profile =
                    stored.unwrap_or_else(|| local_profile(&self.session_profile_id(), &config));
                profile.config.env_vars = config.env_vars.clone();
                profile.config.gateway.max_iterations = config.max_iterations;
                bootstrap(LocalOupOptions {
                    config,
                    profile,
                    data_dir: self.data_dir.clone(),
                    config_home: self.data_dir.clone(),
                    no_retry: false,
                    provider: None,
                    tool_profile: Some(tool_profile),
                    save_episodes: true,
                })
                .await
            })
            .await
            .cloned()
    }
    fn default_cwd(&self) -> &std::path::Path {
        &self.default_cwd
    }

    fn session_store(&self) -> Option<Arc<Mutex<SessionManager>>> {
        self.oup_state
            .get()
            .and_then(|state| state.sessions.clone())
    }

    fn session_profile_id(&self) -> String {
        self.profile
            .clone()
            .filter(|id| !id.contains('/'))
            .unwrap_or_else(|| octos_core::MAIN_PROFILE_ID.to_string())
    }
}

/// Test-support factory that wraps a caller-supplied [`LlmProvider`] (e.g. a
/// `MockLlm`) so the end-to-end ACP integration test can drive the real handler
/// wiring without a network-backed provider or on-disk config.
///
/// Not for production use — production goes through [`ConfigAgentFactory`].
#[cfg(feature = "api")]
#[doc(hidden)]
pub struct TestAgentFactory {
    llm: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    default_cwd: PathBuf,
    /// Optional alternate persistence root. Otherwise the mock runtime uses
    /// memory_dir, normally an isolated temporary directory.
    session_store: Option<Arc<Mutex<SessionManager>>>,
    #[cfg(feature = "api")]
    oup_state: tokio::sync::OnceCell<Arc<crate::api::AppState>>,
}

#[cfg(feature = "api")]
#[doc(hidden)]
impl TestAgentFactory {
    /// Build a factory that hands every session an agent backed by `llm`, with
    /// episodic memory in `memory_dir` and tools rooted at `default_cwd`.
    pub fn new(llm: Arc<dyn LlmProvider>, memory_dir: PathBuf, default_cwd: PathBuf) -> Self {
        Self {
            llm,
            memory_dir,
            default_cwd,
            session_store: None,
            #[cfg(feature = "api")]
            oup_state: tokio::sync::OnceCell::new(),
        }
    }

    /// Persist this factory's sessions under `dir`, so a test can prove a
    /// conversation survives being reloaded into a fresh transport.
    pub fn with_session_store(mut self, dir: &std::path::Path) -> Self {
        self.session_store = SessionManager::open(dir)
            .ok()
            .map(|store| Arc::new(Mutex::new(store)));
        self
    }
}

#[cfg(feature = "api")]
#[async_trait::async_trait]
impl SessionAgentFactory for TestAgentFactory {
    #[cfg(feature = "api")]
    async fn oup_state(&self) -> Result<Arc<crate::api::AppState>> {
        self.oup_state
            .get_or_try_init(|| async {
                use crate::runtime::local_oup::{LocalOupOptions, bootstrap, local_profile};
                let data_dir = match &self.session_store {
                    Some(store) => store.lock().await.data_dir(),
                    None => self.memory_dir.clone(),
                };
                let config = Config {
                    provider: Some(self.llm.provider_name().to_owned()),
                    model: Some(self.llm.model_id().to_owned()),
                    memory: Some(crate::config::MemoryConfig {
                        refresh: Some(crate::config::MemoryRefreshConfig {
                            enabled: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                bootstrap(LocalOupOptions {
                    profile: local_profile(&self.session_profile_id(), &config),
                    config,
                    data_dir: data_dir.clone(),
                    config_home: data_dir,
                    no_retry: true,
                    provider: Some(self.llm.clone()),
                    tool_profile: None,
                    save_episodes: false,
                })
                .await
            })
            .await
            .cloned()
    }
    fn session_store(&self) -> Option<Arc<Mutex<SessionManager>>> {
        self.session_store.clone()
    }

    fn default_cwd(&self) -> &std::path::Path {
        &self.default_cwd
    }
}

/// Test-support transport: the octos ACP **agent** exposed as a
/// `ConnectTo<Client>` so an in-process ACP client can use it directly as its
/// transport (no OS pipes, no subprocess, no network).
///
/// The `agent-client-protocol` runtime treats one endpoint as the "transport"
/// for the other; the agent side is `ConnectTo<Client>` (since `Client` is the
/// `Agent` role's counterpart). The integration test does:
///
/// ```ignore
/// Client.builder()
///     .on_receive_notification(record_updates, ...)
///     .connect_with(OctosAcpAgentTransport::new(factory), |conn| async {
///         conn.send_request(InitializeRequest::new(V1)).block_task().await?;
///         // session/new, session/prompt, assert stop reason ...
///     })
///     .await
/// ```
#[cfg(feature = "api")]
#[doc(hidden)]
pub struct OctosAcpAgentTransport {
    factory: TestAgentFactory,
}

#[cfg(feature = "api")]
#[doc(hidden)]
impl OctosAcpAgentTransport {
    /// Wrap a [`TestAgentFactory`] so it can serve one in-process ACP client.
    pub fn new(factory: TestAgentFactory) -> Self {
        Self { factory }
    }
}

#[cfg(feature = "api")]
impl agent_client_protocol::ConnectTo<Client> for OctosAcpAgentTransport {
    async fn connect_to(
        self,
        client: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
    ) -> std::result::Result<(), AcpError> {
        oup::serve(Arc::new(self.factory), client).await
    }
}

impl AcpCommand {
    /// Build the agent factory `octos acp` serves — without serving it.
    ///
    /// The embedding seam. Everything `octos acp` does before it touches stdio
    /// happens here: context/config resolution, provider and model selection,
    /// and the lazily-built shared agent stack. What comes back is the same
    /// [`SessionAgentFactory`] the stdio path drives, so an embedder that links
    /// octos instead of spawning it gets provider fallbacks, the auth store,
    /// `keychain:` markers, MCP, plugins, skills and memory identically — and
    /// stays in step with the CLI, rather than reimplementing a subset that
    /// drifts.
    ///
    /// `build(cwd)` on the result hands back a runnable [`Agent`] plus its
    /// shutdown flag; drive it however you like.
    ///
    /// Callers embedding this need a tokio runtime, and should note the agent's
    /// tools run with the host process's own privileges, rooted at `cwd`.
    ///
    /// ```ignore
    /// let factory = AcpCommand { provider: Some("anthropic".into()), ..Default::default() }
    ///     .factory()?;
    /// let (agent, shutdown) = factory.build(workspace).await?;
    /// ```
    #[cfg(feature = "api")]
    pub fn factory(&self) -> Result<Arc<dyn SessionAgentFactory>> {
        // Resolve config the same way `octos chat` does.
        let cwd = match &self.cwd {
            Some(c) => c.clone(),
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        let ctx = super::resolve_command_context(self.data_dir.clone())?;
        let data_dir = ctx.data_dir.clone();
        let mut config = if let Some(config_path) = &self.config {
            Config::from_file(config_path)?
        } else if let Some(profile_config) =
            super::chat::load_serve_profile_config(self.profile.as_deref(), &data_dir)?
        {
            profile_config
        } else {
            Config::load_with_context(&cwd, &ctx)?
        };
        super::chat::detach_route_on_provider_override(&mut config, self.provider.as_deref());

        let model = self.model.clone().or(config.model.clone());
        let base_url = self.base_url.clone().or(config.base_url.clone());
        let provider_name = self
            .provider
            .clone()
            .or(config.provider.clone())
            .or_else(|| {
                model
                    .as_deref()
                    .and_then(crate::config::detect_provider)
                    .map(String::from)
            })
            .ok_or_else(|| {
                eyre::eyre!(
                    "no LLM provider configured. Run `octos init` or set provider in config.json"
                )
            })?;

        // The full shared agent stack (provider chain, memory, plugins, MCP,
        // hooks, system prompt, compaction) is assembled lazily on the first
        // `session/new` — see `ConfigAgentFactory`.
        Ok(Arc::new(ConfigAgentFactory {
            config,
            provider_name,
            model,
            base_url,
            data_dir,
            default_cwd: cwd,
            max_iterations: self.max_iterations,
            profile: self.profile.clone(),
            #[cfg(feature = "api")]
            oup_state: tokio::sync::OnceCell::new(),
        }))
    }

    async fn run_async(self) -> Result<()> {
        #[cfg(feature = "api")]
        {
            oup::serve(self.factory()?, Stdio::new())
                .await
                .map_err(|e| eyre::eyre!("ACP connection ended with error: {e}"))
        }
        #[cfg(not(feature = "api"))]
        {
            eyre::bail!(
                "octos acp requires the OUP runtime; rebuild with default features or --features api"
            )
        }
    }
}

/// Build the `initialize` response advertising octos's agent capabilities.
///
/// Per ACP the agent must reply with a protocol version it actually supports —
/// NOT whatever the client requested. This handler implements v1 only, so we
/// echo the client's version only when it is a version we support (V1);
/// otherwise (a newer/unknown version) we reply with the latest version we do
/// support (`ProtocolVersion::LATEST`, currently V1) rather than falsely
/// advertising support for the requested one.
#[cfg(any(feature = "api", test))]
fn build_initialize_response(req: &InitializeRequest) -> InitializeResponse {
    use agent_client_protocol::schema::ProtocolVersion;

    // Prompt capabilities: octos consumes plain text prompt blocks today.
    // Image/audio/embedded-context are left false (v1 extracts text only).
    let prompt = PromptCapabilities::new();
    let caps = AgentCapabilities::new()
        // Advertised because session/load now genuinely restores: it reads the
        // conversation back from SessionManager, which session/prompt writes each
        // turn through to. It stayed false while load was a stub, and must go back
        // to false if that store is ever removed — a client that sees the
        // capability will call it, and a load that silently returns nothing is
        // worse than a method_not_found.
        .load_session(true)
        .prompt_capabilities(prompt);

    let negotiated = if req.protocol_version == ProtocolVersion::V1 {
        req.protocol_version
    } else {
        ProtocolVersion::LATEST
    };
    InitializeResponse::new(negotiated).agent_capabilities(caps)
}

/// Handle `session/new`: build a fresh octos agent and register it.
/// Generate a fresh, unique ACP session id.
#[cfg(any(feature = "api", test))]
fn new_session_id() -> SessionId {
    SessionId::new(format!("octos-{}", uuid::Uuid::new_v4()))
}

/// Extract the plain text from a prompt's ACP content blocks.
///
/// octos's agent loop consumes a single text prompt; we concatenate the text of
/// every `ContentBlock::Text` (and the text payload of embedded text
/// resources), joining multiple blocks with newlines. `ContentBlock::ResourceLink`
/// is BASELINE ACP prompt content (file/context attachments), so we surface each
/// link as a `[Resource: <title|name> (<uri>)]` reference (plus its description on
/// the next line, if any) rather than dropping it. Binary non-text blocks (image,
/// audio) are still skipped in v1 — we advertise text-only prompt capabilities at
/// `initialize`.
#[cfg(any(feature = "api", test))]
fn extract_prompt_text(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Resource(r) => {
                if let agent_client_protocol::schema::v1::EmbeddedResourceResource::TextResourceContents(tr) =
                    &r.resource
                {
                    parts.push(tr.text.clone());
                }
            }
            // Resource links are baseline ACP prompt content (file/context
            // attachments). Surface the reference as usable context text so a
            // prompt made entirely of links still reaches octos with content.
            ContentBlock::ResourceLink(link) => {
                let label = link.title.as_deref().unwrap_or(&link.name);
                let mut part = format!("[Resource: {label} ({})]", link.uri);
                if let Some(desc) = &link.description {
                    part.push('\n');
                    part.push_str(desc);
                }
                parts.push(part);
            }
            // Remaining non-text blocks (image, audio) and any future variants
            // are not consumed by the text-only agent loop. `ContentBlock`
            // is `#[non_exhaustive]`, so a catch-all is required.
            _ => {}
        }
    }
    parts.join("\n")
}

/// Best-effort mapping from an octos tool name to an ACP [`ToolKind`] so clients
/// can render an appropriate icon. Unknown tools fall back to `Other`.
#[cfg(feature = "api")]
fn tool_kind_for(name: &str) -> agent_client_protocol::schema::v1::ToolKind {
    use agent_client_protocol::schema::v1::ToolKind;
    match name {
        "read_file" | "list_dir" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "glob" | "grep" => ToolKind::Search,
        "shell" => ToolKind::Execute,
        "web_search" | "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ProtocolVersion;

    #[test]
    fn should_extract_and_join_text_blocks_when_prompt_has_multiple_blocks() {
        use agent_client_protocol::schema::v1::{ImageContent, TextContent};
        let blocks = vec![
            ContentBlock::Text(TextContent::new("first line")),
            // An image block must be skipped, not crash.
            ContentBlock::Image(ImageContent::new("base64data", "image/png")),
            ContentBlock::Text(TextContent::new("second line")),
        ];
        let text = extract_prompt_text(&blocks);
        assert_eq!(text, "first line\nsecond line");
    }

    #[test]
    fn should_extract_text_from_embedded_text_resource() {
        use agent_client_protocol::schema::v1::{
            EmbeddedResource, EmbeddedResourceResource, TextResourceContents,
        };
        let blocks = vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                "resource body",
                "file:///tmp/x.txt",
            )),
        ))];
        assert_eq!(extract_prompt_text(&blocks), "resource body");
    }

    #[test]
    fn should_include_resource_link_reference_when_prompt_has_resource_link() {
        use agent_client_protocol::schema::v1::ResourceLink;
        // A resource link with a title + description: the reference (title +
        // uri) and the description must both surface in the extracted text.
        let link = ResourceLink::new("main.rs", "file:///repo/src/main.rs")
            .title("Main entrypoint")
            .description("The program entrypoint");
        let blocks = vec![ContentBlock::ResourceLink(link)];
        let text = extract_prompt_text(&blocks);
        assert!(
            text.contains("file:///repo/src/main.rs"),
            "resource-link uri must be surfaced; got {text:?}"
        );
        assert!(
            text.contains("Main entrypoint"),
            "resource-link title must be surfaced; got {text:?}"
        );
        assert!(
            text.contains("The program entrypoint"),
            "resource-link description must be surfaced; got {text:?}"
        );
    }

    #[test]
    fn should_use_resource_link_name_when_title_absent() {
        use agent_client_protocol::schema::v1::ResourceLink;
        // No title -> fall back to the link name; no description -> single line.
        let link = ResourceLink::new("notes.txt", "file:///repo/notes.txt");
        let blocks = vec![ContentBlock::ResourceLink(link)];
        let text = extract_prompt_text(&blocks);
        assert_eq!(text, "[Resource: notes.txt (file:///repo/notes.txt)]");
    }

    #[test]
    fn should_extract_from_string_convertible_block_via_from_impl() {
        // The crate provides `From<Into<String>> for ContentBlock` (Text). This
        // is the ergonomic path used by the reporter mapping above.
        let block: ContentBlock = "just text".into();
        assert_eq!(
            extract_prompt_text(std::slice::from_ref(&block)),
            "just text"
        );
    }

    #[test]
    fn should_build_initialize_response_echoing_protocol_version_and_advertising_text_prompt() {
        let req = InitializeRequest::new(ProtocolVersion::V1);
        let resp = build_initialize_response(&req);
        // We support V1, so a V1 request is echoed back as V1.
        assert_eq!(resp.protocol_version, ProtocolVersion::V1);
        // Text-only prompt capabilities: image/audio/embedded_context all false.
        assert!(!resp.agent_capabilities.prompt_capabilities.image);
        assert!(!resp.agent_capabilities.prompt_capabilities.audio);
        // loadSession is advertised now that session/load genuinely restores a
        // conversation from SessionManager. Advertising it while load was a stub
        // would have been worse than not advertising: a client that sees the
        // capability calls it, and a load that silently returns nothing looks like
        // an agent that has forgotten everything.
        assert!(resp.agent_capabilities.load_session);
    }

    #[test]
    fn should_downgrade_to_latest_supported_version_when_client_requests_newer() {
        // A client requesting a newer/unsupported protocol version must NOT get
        // that version echoed back (that would falsely advertise support this
        // v1-only handler doesn't implement). We reply with the latest version
        // we actually support (`ProtocolVersion::LATEST`, currently V1).
        let newer = ProtocolVersion::from(2u16);
        assert_ne!(newer, ProtocolVersion::V1, "sanity: 2 is not V1");
        let req = InitializeRequest::new(newer);
        let resp = build_initialize_response(&req);
        assert_eq!(resp.protocol_version, ProtocolVersion::LATEST);
        assert_eq!(resp.protocol_version, ProtocolVersion::V1);
    }

    #[test]
    fn should_generate_unique_session_ids() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a.0, b.0, "session ids must be unique");
        assert!(a.0.starts_with("octos-"));
    }
}
