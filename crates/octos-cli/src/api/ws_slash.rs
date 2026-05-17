//! Server-side slash-command interception for the WebSocket UI Protocol
//! v1 chat path.
//!
//! ## Why this module exists
//!
//! Issue [#1013](https://github.com/octos-org/octos/issues/1013) — when
//! the chat transport migrated to UI Protocol v1 over WebSocket (PR
//! #66), the slash-command interception that exists on the
//! gateway/SSE path was not ported. The pre-migration gateway path
//! (`GatewayDispatcher::try_dispatch_session_command` plus
//! `SessionActor::try_handle_command`) runs BEFORE the LLM round-trip
//! and short-circuits commands like `/clear`, `/new slides …`, `/new
//! site …`, `/queue`, `/adaptive`, `/router`, `/status`, `/reset`,
//! `/thinking`. Without this interception on the WS path, every such
//! message reached the LLM and got a conversational response —
//! breaking the slides + sites scaffolding flow on the SPA entirely.
//!
//! ## Design — Approach B (issue #1013 plan)
//!
//! Lift the relevant parser/dispatch logic into a shared helper. Both
//! the gateway path and the WS turn path call the helper before
//! constructing the LLM request. On the gateway path the existing
//! `GatewayDispatcher` chain is left untouched (it's wired into
//! per-actor mutable state — `adaptive_router`, `queue_mode`, etc. —
//! that the WS turn path doesn't carry). On the WS path
//! [`try_dispatch_slash_command`] runs first; if it returns
//! `Some(reply)` the WS turn path persists the reply as an assistant
//! row + emits `turn/completed` and skips the LLM entirely.
//!
//! ## Coverage
//!
//! The helper handles the WS-supportable subset of slash commands:
//!
//! * `/clear` — clears the session ledger via the shared `SessionManager`.
//! * `/new slides <name>` — scaffolds the slides project under the
//!   user's workspace (mirroring the gateway path's behavior at
//!   `gateway_dispatcher.rs:150-183`).
//! * `/new site <preset> …` (and bare `/new site`) — scaffolds the
//!   site project (mirroring `gateway_dispatcher.rs:184-211`).
//! * `/new <topic>` (no template prefix) — synthesises a "session
//!   switched" reply. On the WS transport the SPA controls active
//!   session via URL; this is purely a status acknowledgement.
//! * `/queue`, `/adaptive`, `/router`, `/status`, `/reset`,
//!   `/thinking` — return a "not available on this transport"
//!   acknowledgement. These per-session-actor commands depend on
//!   gateway-only state. The point is to INTERCEPT them so they
//!   don't leak into the LLM context — full WS-side state work is
//!   out of scope for #1013.
//! * Any other `/<unknown>` slash — returns the help / unknown-command
//!   text identical to the gateway path's fallback.
//!
//! Non-slash messages return `None` so the caller falls through to the
//! LLM. Empty / whitespace-only inputs also return `None` (the LLM
//! path itself rejects empties).

use std::path::PathBuf;
use std::sync::Arc;

use octos_bus::SessionManager;
use octos_core::SessionKey;
use tokio::sync::Mutex;

/// References to the per-turn state the WS slash dispatcher needs.
///
/// Holds shared (`Arc<Mutex<…>>`) handles rather than owned values so
/// the helper does not interfere with the rest of the turn's lifetime
/// management.
pub struct SlashCommandContext {
    /// Process-wide [`SessionManager`] — the WS turn path resolves this
    /// from `SessionRuntime.sessions`. Used by `/clear` to wipe the
    /// session ledger via the canonical path.
    pub sessions: Arc<Mutex<SessionManager>>,
    /// The originating session for this turn. Both `/clear` (which
    /// targets the per-session ledger) and `/new slides|site` (which
    /// scaffolds under `<workspace_root>/`) derive their on-disk paths
    /// from this key.
    pub session_id: SessionKey,
    /// Profile data dir — the WS turn path threads this from
    /// `SessionRuntime.profile.data_dir`. Used as the `data_dir`
    /// parameter passed into
    /// [`crate::project_templates::scaffold_site_project`] (skill-dir
    /// lookup) and into [`crate::project_templates::try_activate_*_template`]
    /// (session prompt write).
    pub data_dir: PathBuf,
    /// Active session workspace root — the WS turn path threads this
    /// from `SessionRuntime.workspace_root`. This is the SAME directory
    /// the per-session tool registry is bound to; scaffolding under any
    /// other root would write files OUTSIDE the active sandbox.
    ///
    /// `None` falls back to the conventional
    /// `<data_dir>/users/<encoded_base>/workspace/` layout
    /// (`session_actor.rs::default_workspace_root`) for callers that
    /// never opened a session with a workspace hint. Issue #1015
    /// round-2 fixup: pre-fix the helper always reconstructed the
    /// conventional path, which silently bypassed `workspace_hint`
    /// overrides used by the coding-agent UI.
    pub workspace_root: Option<PathBuf>,
    /// Optional profile id for the site scaffold. The gateway path
    /// derives this from `GatewayDispatcher.dispatch_profile_id`; on
    /// the WS path we plumb the same value from
    /// `SessionKey::profile_id()` / the routed profile id at handshake
    /// time. `None` falls back to `MAIN_PROFILE_ID` to match
    /// `gateway_dispatcher.rs:188`.
    pub profile_id: Option<String>,
}

/// Try to intercept a slash command on the WS turn path. Returns
/// `Some(reply_text)` if the message was a slash command (the WS turn
/// path should persist this string as the assistant reply and emit
/// `turn/completed` instead of going to the LLM), or `None` if the
/// message is not a slash command and should fall through to the
/// normal LLM path.
///
/// See the module-level doc comment for full coverage / rationale.
pub async fn try_dispatch_slash_command(
    message: &str,
    ctx: &SlashCommandContext,
) -> Option<String> {
    let trimmed = message.trim();
    if !trimmed.starts_with('/') {
        // Non-slash messages (including whitespace-only) flow to the LLM.
        return None;
    }

    // Split into command + remainder. `parts.first()` is the
    // `/<cmd>` token; `name_arg` is the trimmed suffix (e.g.
    // "slides demo-deck" for `/new slides demo-deck`).
    let mut split = trimmed.splitn(2, char::is_whitespace);
    let cmd = split.next().unwrap_or(trimmed);
    let name_arg = split.next().unwrap_or("").trim();

    match cmd {
        "/clear" => Some(handle_clear(ctx).await),
        "/new" => Some(handle_new(ctx, name_arg).await),
        // Session-management commands not supported on the WS turn
        // transport (the SPA navigates via URL/session_id). Intercept
        // so they don't reach the LLM, surface a brief explanatory
        // reply.
        "/s" | "/switch" | "/sessions" | "/back" | "/b" | "/delete" | "/d" | "/soul" => {
            Some(format!(
                "`{cmd}` is not available over the web chat transport. \
                 Use the session picker in the sidebar to navigate."
            ))
        }
        // Per-session-actor commands. Gateway carries the state these
        // mutate (queue mode, adaptive router, etc.); the WS turn
        // path doesn't. Intercept so they don't leak into LLM
        // context.
        "/queue" | "/adaptive" | "/router" | "/status" | "/reset" | "/thinking" => Some(format!(
            "`{cmd}` is not yet wired on the web chat transport \
             (gateway-only for now). Issue #1013 follow-up will surface \
             the matching control in the SPA."
        )),
        _ => Some(unknown_command_help()),
    }
}

/// `/clear` — wipe the current session's history via the canonical
/// `SessionManager::clear` path. Mirrors `gateway_dispatcher.rs:116-126`.
async fn handle_clear(ctx: &SlashCommandContext) -> String {
    match ctx.sessions.lock().await.clear(&ctx.session_id).await {
        Ok(()) => "Session cleared.".to_string(),
        Err(error) => {
            tracing::warn!(
                session = %ctx.session_id.0,
                error = %error,
                "ws slash: session clear failed"
            );
            format!("Failed to clear session: {error}")
        }
    }
}

/// `/new [name|template]` — bare `/new` clears the session; `/new
/// slides <name>` and `/new site <preset>` scaffold the matching
/// project template under `<data_dir>/users/<encoded_base>/workspace/`.
/// Other names emit a "switched to session: <name>" acknowledgement.
///
/// Mirrors `gateway_dispatcher.rs:107-220` minus the gateway-only
/// `active_sessions.switch_to` + `touch_user_session` bookkeeping
/// (which doesn't apply to the WS transport — see module-level doc).
async fn handle_new(ctx: &SlashCommandContext, name_arg: &str) -> String {
    if name_arg.is_empty() {
        // Bare `/new` matches the gateway path's `/clear` shape — wipe
        // history. The SPA already provides session switching via the
        // sidebar so `name_arg.is_empty()` is the only branch where
        // `/new` has a meaningful side effect on this transport.
        return handle_clear(ctx).await;
    }

    if let Err(reason) = octos_bus::validate_topic_name(name_arg) {
        return format!("Invalid session name: {reason}");
    }

    // Round-2 fixup (#1015 codex review BLOCKING 2): use the active
    // session's `workspace_root` when threaded through. This is the
    // SAME directory the per-session tool registry is bound to — using
    // anything else (e.g. a reconstructed `data_dir/users/<base>/workspace`
    // when the caller actually opened the session with a workspace
    // hint) would scaffold OUTSIDE the active tool sandbox.
    let workspace_root: PathBuf = ctx
        .workspace_root
        .clone()
        .unwrap_or_else(|| default_workspace_root_for(&ctx.data_dir, &ctx.session_id));
    if let Err(error) = std::fs::create_dir_all(&workspace_root) {
        tracing::warn!(
            session = %ctx.session_id.0,
            workspace = %workspace_root.display(),
            error = %error,
            "ws slash: failed to create workspace root for /new"
        );
    }

    // Round-2 fixup (#1015 codex review BLOCKING 1): normalize the
    // plural `sites` alias to the canonical singular `site` form
    // before downstream parsing. The SPA's sites-chat.tsx canonically
    // emits `/new site <preset>` (singular) matching the gateway
    // path, but the slides equivalent is plural (`/new slides …`) and
    // power users / parallel frontends typing `/new sites astro` by
    // analogy would otherwise fall into the generic switch arm and
    // never trigger the scaffold. Aliasing here lets one branch
    // handle both forms — the downstream
    // [`crate::project_templates::site_preset_from_topic`] parser only
    // recognises the singular form, so we hand it the normalized
    // string.
    let normalized_topic = if name_arg == "sites" {
        std::borrow::Cow::Borrowed("site")
    } else if let Some(rest) = name_arg.strip_prefix("sites ") {
        std::borrow::Cow::Owned(format!("site {rest}"))
    } else {
        std::borrow::Cow::Borrowed(name_arg)
    };
    let topic: &str = normalized_topic.as_ref();

    if topic == "slides" || topic.starts_with("slides ") {
        match crate::project_templates::try_activate_slides_template(&ctx.data_dir, topic) {
            Some(template_reply) => {
                let project_name = topic.strip_prefix("slides").unwrap_or("").trim();
                let project_name = if project_name.is_empty() {
                    "untitled"
                } else {
                    project_name
                };
                match crate::project_templates::scaffold_slides_project(
                    &workspace_root,
                    project_name,
                ) {
                    Ok(_) => template_reply,
                    Err(error) => {
                        tracing::warn!(
                            topic = topic,
                            error = %error,
                            "ws slash: slides scaffold failed"
                        );
                        format!("{template_reply}\n\nSlides git/bootstrap failed: {error}")
                    }
                }
            }
            None => format!("Switched to session: {name_arg}"),
        }
    } else if topic == "site" || topic.starts_with("site ") {
        let _ = crate::project_templates::try_activate_site_template(&ctx.data_dir, topic);
        let profile_id = ctx
            .profile_id
            .clone()
            .unwrap_or_else(|| octos_core::MAIN_PROFILE_ID.to_string());
        match crate::project_templates::scaffold_site_project(
            &workspace_root,
            &profile_id,
            ctx.session_id.chat_id(),
            topic,
            &ctx.data_dir,
        ) {
            Ok(metadata) => crate::project_templates::site_creation_reply(&metadata),
            Err(error) => {
                tracing::warn!(
                    topic = topic,
                    error = %error,
                    "ws slash: site scaffold failed"
                );
                format!("Site scaffold failed: {error}")
            }
        }
    } else {
        format!("Switched to session: {name_arg}")
    }
}

/// Default workspace root layout when the caller did not thread a
/// session workspace_root through `SlashCommandContext`. Mirrors
/// `runtime::session::resolve_workspace_root`'s fallback so the slash
/// helper and the per-session tool registry agree on the same root.
fn default_workspace_root_for(data_dir: &std::path::Path, session_id: &SessionKey) -> PathBuf {
    let encoded_base = octos_bus::session::encode_path_component(session_id.base_key());
    data_dir
        .join("users")
        .join(encoded_base)
        .join("workspace")
}

/// Unknown-command help text. Matches the wording in
/// `session_actor.rs::try_handle_command`'s `_ =>` arm so users see
/// the same help on both transports.
fn unknown_command_help() -> String {
    "Unknown command. Available commands:\n\
     /new [name] — start a new session\n\
     /clear — clear the current session\n\
     /new slides <name> — scaffold a slides project\n\
     /new site <preset> — scaffold a site project\n\
     /help — show this help"
        .to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::Message;
    use tempfile::TempDir;

    /// Build a fresh, isolated context. Returns the tempdir handle so
    /// the caller can keep the fixture filesystem alive.
    async fn setup() -> (SlashCommandContext, TempDir, SessionKey) {
        let tmp = TempDir::new().unwrap();
        let session_key = SessionKey::new("api", "unit-test");
        let sessions = Arc::new(Mutex::new(SessionManager::open(tmp.path()).unwrap()));
        let ctx = SlashCommandContext {
            sessions,
            session_id: session_key.clone(),
            data_dir: tmp.path().to_path_buf(),
            workspace_root: None,
            profile_id: None,
        };
        (ctx, tmp, session_key)
    }

    #[tokio::test]
    async fn should_pass_through_non_slash_messages() {
        let (ctx, _tmp, _key) = setup().await;
        assert!(
            try_dispatch_slash_command("hello world", &ctx)
                .await
                .is_none()
        );
        assert!(try_dispatch_slash_command("", &ctx).await.is_none());
        assert!(try_dispatch_slash_command("  \n  ", &ctx).await.is_none());
        // Mid-text slashes are not slash commands.
        assert!(
            try_dispatch_slash_command("read /etc/hosts", &ctx)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn should_intercept_clear_and_wipe_history() {
        let (ctx, _tmp, key) = setup().await;
        {
            let mut mgr = ctx.sessions.lock().await;
            mgr.add_message(&key, Message::user("hello")).await.unwrap();
        }
        let reply = try_dispatch_slash_command("/clear", &ctx).await.unwrap();
        assert!(reply.to_ascii_lowercase().contains("clear"));

        let mut mgr = ctx.sessions.lock().await;
        let history = mgr.get_or_create(&key).await.get_history(50);
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn should_scaffold_slides_under_user_workspace() {
        let (ctx, tmp, key) = setup().await;
        let reply = try_dispatch_slash_command("/new slides demo", &ctx)
            .await
            .unwrap();
        assert!(reply.contains("demo"));

        let encoded = octos_bus::session::encode_path_component(key.base_key());
        let project = tmp
            .path()
            .join("users")
            .join(&encoded)
            .join("workspace")
            .join("slides")
            .join("demo");
        assert!(
            project.is_dir(),
            "expected scaffold at {}",
            project.display()
        );
        assert!(project.join("script.js").is_file());
    }

    #[tokio::test]
    async fn should_return_unknown_help_for_garbage_slash() {
        let (ctx, _tmp, _key) = setup().await;
        let reply = try_dispatch_slash_command("/blorpus quack", &ctx)
            .await
            .unwrap();
        let lc = reply.to_ascii_lowercase();
        assert!(lc.contains("unknown") || lc.contains("available"));
    }

    #[tokio::test]
    async fn should_intercept_session_actor_style_commands() {
        let (ctx, _tmp, _key) = setup().await;
        for cmd in [
            "/queue",
            "/adaptive",
            "/router",
            "/status",
            "/reset",
            "/thinking",
        ] {
            assert!(
                try_dispatch_slash_command(cmd, &ctx).await.is_some(),
                "{cmd} must be intercepted"
            );
        }
    }
}
