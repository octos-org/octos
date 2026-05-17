//! Issue #1013 — guards that the WebSocket `/chat` turn path intercepts
//! slash commands BEFORE the LLM round-trip, matching the gateway path's
//! existing semantics.
//!
//! ## What this guards
//!
//! When the chat transport migrated to UI Protocol v1 over WebSocket
//! (PR #66), the slash-command interception in `session_actor.rs::
//! try_handle_command` and `gateway_dispatcher.rs::
//! try_dispatch_session_command` was lost on the WS path. Messages like
//! `/new slides demo-deck`, `/new sites`, `/clear`, `/queue`,
//! `/adaptive`, etc. reached the LLM and produced a conversational
//! response instead of running the intended slash-command side effect
//! (scaffolding, session clear, etc.). This broke slides + sites
//! scaffolding on the SPA entirely.
//!
//! Fix (#1013): a shared helper `try_dispatch_slash_command` parses the
//! prompt and runs the appropriate side effect (slides/site scaffold,
//! session clear, etc.). The helper is invoked from both the gateway
//! and the WS turn paths. The gateway path was already correct; these
//! tests guard the WS-path wiring.
//!
//! ## Test shape
//!
//! These tests exercise the helper directly. The WS turn path calls
//! the same entry point right before the LLM construction in
//! `run_standalone_turn`. The contract under test:
//!
//! 1. `/new slides <name>` → scaffold runs, helper returns Some(reply
//!    text with scaffold reply); the WS path would persist + emit
//!    `turn/completed` instead of going to the LLM.
//! 2. `/clear` → the session ledger is cleared, helper returns
//!    `Some(reply)`.
//! 3. A non-slash message → helper returns `None`, the WS path falls
//!    through to the LLM (regression guard).
//! 4. `/garbage` (unknown slash) → helper returns `Some(reply)` (the
//!    help / unknown-command text), still NOT routed to the LLM.

#![cfg(feature = "api")]

use std::sync::Arc;

use octos_cli::api::ws_slash::{SlashCommandContext, try_dispatch_slash_command};
use octos_core::{Message, SessionKey};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Build a fresh, isolated `SlashCommandContext` rooted at a tempdir,
/// with a real (on-disk) `SessionManager` plus a `SessionKey`. Returns
/// the tempdir so the caller can keep it alive for the duration of the
/// test (dropping it before the assertions would tear down the
/// filesystem fixture).
async fn setup_ctx() -> (SlashCommandContext, TempDir, SessionKey) {
    let tmp = TempDir::new().unwrap();
    let session_key = SessionKey::new("api", "web-test-chat");
    let sessions = Arc::new(Mutex::new(
        octos_bus::SessionManager::open(tmp.path()).unwrap(),
    ));
    let ctx = SlashCommandContext {
        sessions: sessions.clone(),
        session_id: session_key.clone(),
        data_dir: tmp.path().to_path_buf(),
        workspace_root: None,
        profile_id: None,
    };
    (ctx, tmp, session_key)
}

// ── Scenario 1 — /new slides scaffolds ──────────────────────────────────

/// Issue #1013 — `/new slides <name>` must run the slides scaffold side
/// effect on the WS path, not be conversationally answered by the LLM.
///
/// Pre-fix: this returns `None` (helper does not exist), and the WS
/// path falls straight through to the LLM with a conversational
/// "Sure, let me help you with slides…" instead of creating any files.
///
/// Post-fix: the scaffold runs in the workspace + the helper returns a
/// non-empty reply, which the WS turn path persists as an assistant
/// `MessagePersisted` event and concludes with `turn/completed`.
#[tokio::test]
async fn should_scaffold_slides_when_new_slides_command_arrives_on_ws() {
    let (ctx, tmp, session_key) = setup_ctx().await;

    let reply = try_dispatch_slash_command("/new slides demo-deck", &ctx).await;

    let reply = reply.expect("/new slides demo-deck must be intercepted on WS path");
    assert!(
        reply.contains("demo-deck"),
        "scaffold reply must reference the project name, got: {reply}"
    );

    // The scaffold lives under `<data_dir>/users/<encoded_base>/workspace/slides/<slug>`.
    let encoded_base = octos_bus::session::encode_path_component(session_key.base_key());
    let project_dir = tmp
        .path()
        .join("users")
        .join(&encoded_base)
        .join("workspace")
        .join("slides")
        .join("demo-deck");

    assert!(
        project_dir.is_dir(),
        "slides project dir must exist after /new slides demo-deck — got missing {}",
        project_dir.display()
    );
    assert!(project_dir.join("script.js").is_file());
    assert!(project_dir.join("memory.md").is_file());
}

// ── Scenario 2 — /clear clears the session ──────────────────────────────

#[tokio::test]
async fn should_clear_session_history_when_clear_command_arrives_on_ws() {
    let (ctx, _tmp, session_key) = setup_ctx().await;

    // Pre-populate one user message so /clear has something to wipe.
    {
        let mut mgr = ctx.sessions.lock().await;
        mgr.add_message(&session_key, Message::user("hello first turn"))
            .await
            .unwrap();
        let history = mgr.get_or_create(&session_key).await.get_history(50);
        assert!(!history.is_empty(), "fixture: seed message must exist");
    }

    let reply = try_dispatch_slash_command("/clear", &ctx).await;

    let reply = reply.expect("/clear must be intercepted on WS path");
    assert!(
        reply.to_ascii_lowercase().contains("clear")
            || reply.to_ascii_lowercase().contains("reset"),
        "/clear reply must indicate session was reset, got: {reply}"
    );

    let mut mgr = ctx.sessions.lock().await;
    let history = mgr.get_or_create(&session_key).await.get_history(50);
    assert!(
        history.is_empty(),
        "session history must be empty after /clear, got {} message(s)",
        history.len()
    );
}

// ── Scenario 3 — non-slash messages still reach the LLM ─────────────────

/// Regression guard: a normal conversational message MUST NOT be
/// intercepted by the helper. The WS turn path checks the helper, and
/// when it returns `None`, falls through to the LLM construction.
#[tokio::test]
async fn should_not_intercept_non_slash_messages() {
    let (ctx, _tmp, _session_key) = setup_ctx().await;

    let reply = try_dispatch_slash_command("Hello, slide me X", &ctx).await;

    assert!(
        reply.is_none(),
        "non-slash message must NOT be intercepted (helper must return None so \
         the WS turn path falls through to the LLM); got synthesized reply: {reply:?}"
    );

    // Edge case — a message that contains a slash mid-text but doesn't start
    // with one must also pass through.
    let reply2 = try_dispatch_slash_command("describe the /api/foo endpoint", &ctx).await;
    assert!(
        reply2.is_none(),
        "mid-text slash must not trigger interception, got: {reply2:?}"
    );

    // Edge case — whitespace-only / empty input is not a slash command and
    // must fall through to the LLM (the LLM path itself rejects empties).
    let reply3 = try_dispatch_slash_command("   ", &ctx).await;
    assert!(reply3.is_none(), "whitespace must not be intercepted");
}

// ── Scenario 4 — unknown slash command handled gracefully ───────────────

#[tokio::test]
async fn should_handle_unknown_slash_command_gracefully() {
    let (ctx, _tmp, _session_key) = setup_ctx().await;

    let reply = try_dispatch_slash_command("/garbage", &ctx).await;

    let reply = reply.expect("unknown slash command must still be intercepted (not routed to LLM)");
    assert!(
        !reply.trim().is_empty(),
        "unknown-slash reply must contain user-facing text (help/unknown), got empty string"
    );
    let lc = reply.to_ascii_lowercase();
    assert!(
        lc.contains("unknown") || lc.contains("available") || lc.contains("help"),
        "unknown-slash reply should mention 'unknown' or list available commands, got: {reply}"
    );
}

// ── Scenario 5 — /queue and /adaptive must also be intercepted ──────────

/// Issue #1013 explicitly calls out `/queue` and `/adaptive` as part of
/// the set that pre-fix reached the LLM. These don't have full WS-side
/// state (the per-session-actor queue mode and AdaptiveRouter live on
/// the gateway transport), but they MUST still be intercepted so they
/// don't leak the slash text into LLM context.
#[tokio::test]
async fn should_intercept_session_actor_style_commands_on_ws() {
    let (ctx, _tmp, _session_key) = setup_ctx().await;

    for cmd in [
        "/queue",
        "/adaptive",
        "/router",
        "/status",
        "/reset",
        "/thinking",
    ] {
        let reply = try_dispatch_slash_command(cmd, &ctx).await;
        assert!(
            reply.is_some(),
            "{cmd} must be intercepted on WS path so it doesn't reach the LLM"
        );
    }
}

// ── Scenario 6 — `/new sites <preset>` plural form (round-2 fixup #1015) ──

/// Round-2 codex review: `/new sites <preset>` (plural) MUST hit the
/// site scaffold branch, not the generic "Switched to session: …"
/// fallback. The SPA-facing call (`sites-chat.tsx`) and the gateway
/// path use the singular `site` form, but power-users / parallel
/// frontends mirror the slides path (`/new slides …` plural) and type
/// `/new sites astro` by analogy. Pre-fix the plural form fell into
/// the generic `_ => format!("Switched to session: {name_arg}")` arm
/// and never scaffolded.
///
/// We cannot actually scaffold in unit tests — the site bootstrap
/// requires a `mofa-site` skill dir under `data_dir`, which the
/// fixture tempdir does not provide. The distinguishing observable
/// for "the right branch was taken" is therefore the failure message
/// shape: when the scaffold can't complete it surfaces "Site scaffold
/// failed" (with the underlying error). The generic fallback would
/// instead say "Switched to session: sites astro" — completely
/// different wording.
#[tokio::test]
async fn should_scaffold_site_when_new_sites_preset_command_arrives_on_ws() {
    let (ctx, _tmp, _session_key) = setup_ctx().await;

    let reply = try_dispatch_slash_command("/new sites astro", &ctx).await;
    let reply = reply.expect("/new sites astro must be intercepted on WS path");

    // The reply must indicate the SITE scaffold branch ran. Either it
    // succeeded (some test environments may have a mofa-site skill
    // resolvable) or it failed with the scaffold-specific error
    // wording. It MUST NOT be the generic switch-session reply.
    let succeeded = reply.contains("Site project") && reply.contains("created");
    let failed_at_scaffold =
        reply.contains("Site scaffold failed") || reply.contains("site project");
    assert!(
        succeeded || failed_at_scaffold,
        "/new sites <preset> must take the site-scaffold branch, got: {reply}"
    );
    assert!(
        !reply.starts_with("Switched to session: sites"),
        "/new sites <preset> must NOT fall into the generic switch-session arm, got: {reply}"
    );
}

// ── Scenario 7 — session workspace_root override (round-2 fixup #1015) ──

/// Round-2 codex review: the slash helper must scaffold under the
/// session's actual `workspace_root` — NOT the conventional
/// `<data_dir>/users/<encoded>/workspace` reconstruction. When a
/// session is bootstrapped with a `workspace_hint` (the coding-agent
/// flow), `session_runtime.workspace_root` points at the hinted path
/// and ALL tools operate there. If the slash helper reconstructs a
/// different path it can scaffold OUTSIDE the active tool sandbox.
///
/// The fix threads `workspace_root` through `SlashCommandContext` so
/// the helper uses the same root the tools use.
#[tokio::test]
async fn should_use_session_workspace_root_when_override_present() {
    let tmp = TempDir::new().unwrap();
    let custom_workspace = tmp.path().join("custom-coding-root");
    std::fs::create_dir_all(&custom_workspace).unwrap();

    let session_key = SessionKey::new("api", "coding-test");
    let sessions = Arc::new(Mutex::new(
        octos_bus::SessionManager::open(tmp.path()).unwrap(),
    ));
    let ctx = SlashCommandContext {
        sessions: sessions.clone(),
        session_id: session_key.clone(),
        data_dir: tmp.path().to_path_buf(),
        workspace_root: Some(custom_workspace.clone()),
        profile_id: None,
    };

    let reply = try_dispatch_slash_command("/new slides hint-deck", &ctx).await;
    let reply = reply.expect("/new slides hint-deck must be intercepted");
    assert!(reply.contains("hint-deck"));

    // The scaffold must land under the CUSTOM workspace, NOT the
    // conventional users/<encoded>/workspace reconstruction.
    let scaffolded = custom_workspace.join("slides").join("hint-deck");
    assert!(
        scaffolded.is_dir(),
        "slides scaffold must land in the session workspace_root override, got missing {}",
        scaffolded.display()
    );
    assert!(scaffolded.join("script.js").is_file());

    // Negative: the conventional fallback path must NOT exist.
    let encoded_base = octos_bus::session::encode_path_component(session_key.base_key());
    let conventional = tmp
        .path()
        .join("users")
        .join(&encoded_base)
        .join("workspace")
        .join("slides")
        .join("hint-deck");
    assert!(
        !conventional.exists(),
        "scaffold leaked into the data_dir conventional path {} — \
         workspace_root override was ignored",
        conventional.display()
    );
}
