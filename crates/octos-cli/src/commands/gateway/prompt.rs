//! System prompt construction for gateway mode.

use std::path::Path;

use octos_agent::SkillsLoader;
use octos_memory::MemoryStore;

use crate::persona_service::PersonaService;

/// Build the system prompt with bootstrap files, memory context, and skills.
///
pub async fn build_system_prompt(
    base: Option<&str>,
    data_dir: &Path,
    project_dir: &Path,
    memory_store: &MemoryStore,
    skills_loader: &SkillsLoader,
    tool_config: &octos_agent::ToolConfigStore,
) -> String {
    let compiled = include_str!("../../prompts/gateway_default.txt");
    let runtime = super::super::load_prompt("gateway", compiled);
    let mut prompt = base.unwrap_or(&runtime).to_string();

    // Inject current date so the model knows "今年" = which year
    let today = chrono::Local::now().format("%Y-%m-%d");
    prompt.push_str(&format!("\n\nCurrent date: {today}"));

    // Inject platform guidance so the model doesn't emit Unix-only shell
    // commands on Windows hosts.
    #[cfg(windows)]
    {
        prompt.push_str(
            "\n\n## Runtime Platform\n\n\
             Current host OS: Windows.\n\
             If you use the shell tool, write Windows cmd.exe-compatible commands only.\n\
             Do NOT use Unix-only commands like `ps`, `grep`, `head`, `rm`, `ls`, `cat`, `which`, or `bash`.\n\
             Prefer built-in tools (`glob`, `grep`, `list_dir`, `read_file`, `deep_search`, `deep_crawl`, `web_search`, `web_fetch`) over shell whenever possible.\n\
             If a task depends on a tool or binary that is not available on this host, say so explicitly and do not retry via shell.",
        );
    }

    // Inject dynamically generated persona (from persona.md) if available
    if let Some(persona) = PersonaService::read_persona(data_dir) {
        prompt.push_str("\n\n## Communication Style\n\n");
        prompt.push_str(&persona);
    }

    // Append bootstrap files (AGENTS.md, SOUL.md, USER.md, etc.)
    let bootstrap = super::super::load_bootstrap_files(project_dir);
    if !bootstrap.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&bootstrap);
    }

    // Per-user soul override (takes precedence over shared SOUL.md)
    if let Some(user_soul) = crate::soul_service::read_soul(data_dir) {
        prompt.push_str("\n\n## Soul\n\n");
        prompt.push_str(&user_soul);
    }

    // Append memory context
    let memory_ctx = memory_store.get_memory_context().await;
    if !memory_ctx.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&memory_ctx);
    }

    // Append memory bank summary (entity abstracts)
    let bank_summary = memory_store.get_bank_summary().await;
    if !bank_summary.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&bank_summary);
    }

    // Append always-on skills
    if let Ok(always_names) = skills_loader.get_always_skills().await {
        if !always_names.is_empty() {
            if let Ok(skills_content) = skills_loader.load_skills_for_context(&always_names).await {
                if !skills_content.is_empty() {
                    prompt.push_str("\n\n## Active Skills\n\n");
                    prompt.push_str(&skills_content);
                }
            }
        }
    }

    // Append skills summary
    if let Ok(summary) = skills_loader.build_skills_summary().await {
        if !summary.is_empty() {
            prompt.push_str("\n\n## Available Skills\n\n");
            prompt.push_str(&summary);
        }
    }

    // Append tool preferences summary
    let config_summary = tool_config.summary().await;
    if !config_summary.is_empty() {
        prompt.push_str("\n\n## Tool Preferences\n\n");
        prompt.push_str(&config_summary);
    }

    prompt
}

/// Extract a string value from channel settings JSON, with a default fallback.
#[cfg(any(
    feature = "telegram",
    feature = "discord",
    feature = "slack",
    feature = "whatsapp",
    feature = "email",
    feature = "feishu",
    feature = "twilio",
    feature = "wecom",
    feature = "wecom-bot",
    feature = "line",
    feature = "matrix",
    feature = "qq-bot",
    feature = "wechat"
))]
pub fn settings_str(settings: &serde_json::Value, key: &str, default: &str) -> String {
    settings
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

#[cfg(test)]
mod tests {
    //! Regression tests for the compiled-in gateway system prompt.
    //!
    //! These tests assert on the *bundled* prompt string at
    //! `crates/octos-cli/src/prompts/gateway_default.txt` (loaded via
    //! `include_str!` in [`build_system_prompt`]). They lock in the B6
    //! soak-bug fix where `deepseek-v4-pro` answered Chinese news prompts
    //! (`查一下今日新闻头条`) with the 1/2/3 search menu instead of calling
    //! the registered `news_fetch` specialist tool. The previous prompt
    //! had a single catch-all "ALL other search/lookup requests" rule
    //! that mandated the menu for every `查一下 / 搜一下 / 帮我查` prompt,
    //! even when a domain-specific tool matched.
    //!
    //! The fix:
    //!
    //! 1. Adds an `ACT-DIRECTLY for registered specialist tools` rule
    //!    BEFORE the menu, naming `news_fetch`, `get_weather`, `get_time`,
    //!    `podcast_generate`, `voice_synthesize`, `run_pipeline`.
    //! 2. Narrows the menu catch-all to "open-ended research/lookup
    //!    requests WITHOUT a matching specialist tool", with an explicit
    //!    counter-example pointing at the news case.
    //!
    //! These assertions are deliberately substring-based (not exact
    //! match) so the prompt can be edited around the rule without
    //! breaking the test — but the load-bearing phrases must stay.

    const PROMPT: &str = include_str!("../../prompts/gateway_default.txt");

    #[test]
    fn should_have_act_directly_specialist_tools_section() {
        assert!(
            PROMPT.contains("ACT-DIRECTLY for registered specialist tools"),
            "prompt is missing the ACT-DIRECTLY specialist-tools header; \
             the B6 soak fix relied on this section to stop the gateway \
             from asking 1/2/3 for queries like 查一下今日新闻头条"
        );
    }

    #[test]
    fn should_route_news_queries_to_news_fetch_directly() {
        assert!(
            PROMPT.contains("`news_fetch`"),
            "prompt must mention `news_fetch` as the act-directly target \
             for news-headline queries (B6 soak fix)"
        );
        // The Chinese trigger keywords for news must be present so the
        // model picks `news_fetch` for prompts like "查一下今日新闻头条".
        for keyword in ["新闻", "头条", "今日新闻"] {
            assert!(
                PROMPT.contains(keyword),
                "prompt must list `{keyword}` as a news trigger keyword \
                 so the gateway routes Chinese news queries to news_fetch \
                 instead of the 1/2/3 menu"
            );
        }
    }

    #[test]
    fn should_route_weather_and_time_queries_directly() {
        assert!(
            PROMPT.contains("`get_weather`"),
            "prompt must mention `get_weather` for weather queries"
        );
        assert!(
            PROMPT.contains("`get_time`"),
            "prompt must mention `get_time` for time/date queries"
        );
    }

    #[test]
    fn should_route_podcast_and_tts_to_spawn_specialists() {
        assert!(
            PROMPT.contains("`podcast_generate`"),
            "prompt must mention `podcast_generate` for podcast requests"
        );
        assert!(
            PROMPT.contains("`voice_synthesize`"),
            "prompt must mention `voice_synthesize` for TTS / read-aloud \
             requests"
        );
    }

    #[test]
    fn should_reconcile_grounding_rule_with_news_fetch_preference() {
        // The Grounding Rules historically listed "news" alongside other
        // real-time data routed to `web_search` / `web_fetch`. That
        // contradicted the ACT-DIRECTLY specialist-tools rule (codex
        // review on d8eebec9, P2). The reconciled wording must prefer
        // `news_fetch` when it is registered.
        assert!(
            PROMPT.contains(
                "prefer the `news_fetch` specialist tool when it is registered"
            ),
            "Grounding Rules must explicitly prefer news_fetch over \
             web_search/web_fetch when news_fetch is registered (codex \
             P2 follow-up to B6 fix)"
        );
        // The old conflicting phrasing — "news, current events" lumped
        // into the web_search bucket — must not return.
        assert!(
            !PROMPT.contains(
                "(stock prices, sports scores, exchange rates, news, current events"
            ),
            "the unqualified 'news' entry in the web_search list must \
             not be reintroduced (codex P2 regression guard)"
        );
    }

    #[test]
    fn should_narrow_menu_rule_to_no_specialist_tool_match() {
        // The narrow rule must reference "WITHOUT a matching specialist
        // tool" so the 1/2/3 menu only fires for open-ended research.
        assert!(
            PROMPT.contains("WITHOUT a matching specialist tool"),
            "the menu catch-all must be narrowed to 'open-ended \
             research/lookup requests WITHOUT a matching specialist \
             tool' (B6 fix); the previous wording made the menu fire \
             for ALL 查一下/搜一下 prompts"
        );
        // The old broken wording must NOT come back: "For ALL other
        // search/lookup requests ... ALWAYS ask the user to pick 1/2/3"
        // without the specialist-tool carve-out.
        assert!(
            !PROMPT.contains(
                "For ALL other search/lookup requests (including \
                 \"查一下\", \"搜一下\", \"帮我查\", \"search for\"), \
                 ALWAYS ask the user to pick 1/2/3 first."
            ),
            "the unconditional 'ALL other search/lookup requests' menu \
             rule must not be reintroduced (B6 regression guard)"
        );
    }
}
