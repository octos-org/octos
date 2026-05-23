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
    fn should_call_podcast_generate_directly_without_voice_probe() {
        // NEW-05 round-3 (codex recommendation): three rounds of prompt
        // strengthening could not convince `deepseek-v4-pro` to follow
        // through after `podcast_voices` — the model kept stopping at
        // the voice list. The fix is to make `podcast_generate`
        // self-contained: the prompt names the preset voices the skill
        // ships with, and the model writes them straight into the
        // markdown script — no `podcast_voices` precondition required.
        // The verified manifest (mofa-podcast 0.4.5) does NOT require a
        // top-level `voice` argument either, so this is purely a prompt
        // change.
        //
        // codex round-4 follow-up: the previous round-3 nudges
        // ("Do NOT call `podcast_voices` first", "voice list is NOT a
        // precondition") still surfaced the `podcast_voices` token on
        // the generation rules. Some literal-instruction models latched
        // onto the negation and probed anyway. The generation bullets
        // must now scrub the `podcast_voices` token entirely and rely
        // on a generic "no separate voice-listing step is required"
        // phrasing.
        assert!(
            PROMPT.contains("`podcast_generate` DIRECTLY"),
            "prompt must instruct the model to call `podcast_generate` \
             DIRECTLY for podcast generation (NEW-05 round-3 fix); the \
             previous voice-probe workflow stalled deepseek-v4-pro on \
             the voice list"
        );
        // Round-4 regression guard: the generation surface must NOT
        // mention `podcast_voices` (positively or negatively). The
        // only allowed mention is the explicit "Podcast voice list
        // only" carve-out (see
        // `should_preserve_voice_list_only_route_for_explicit_listing`).
        assert!(
            !PROMPT.contains("Do NOT call `podcast_voices` first"),
            "round-4 fix: the negated `podcast_voices` nudge must not \
             return on the generation surface — the token itself was \
             enough to make literal-instruction models probe anyway. \
             Use a generic 'no separate voice-listing step is \
             required' phrasing instead."
        );
        assert!(
            !PROMPT.contains("voice list is NOT a precondition"),
            "round-4 fix: the 'voice list is NOT a precondition' \
             phrasing must not return — round-4 strips voice-listing \
             references off the generation path entirely (the \
             dedicated voice-list bullet still handles explicit \
             listing intent)"
        );
        assert!(
            !PROMPT.contains("voice list is NOT required for generation"),
            "round-4 fix: the 'voice list is NOT required for \
             generation' phrasing must not return — round-4 removes \
             voice-listing references from the generation path"
        );
        assert!(
            PROMPT.contains("no separate voice-listing step is required"),
            "round-4 fix: the generation surface must use the generic \
             'no separate voice-listing step is required' wording to \
             discourage a separate listing call without naming \
             `podcast_voices`"
        );
        // Round-4 structural guard: count `podcast_voices` mentions
        // and ensure there is exactly one (the dedicated voice-list
        // bullet). Any extra mention must live in a "list voices"
        // carve-out, but right now we only ship one such carve-out.
        let mentions = PROMPT.matches("podcast_voices").count();
        assert_eq!(
            mentions, 1,
            "expected exactly one `podcast_voices` mention (the \
             dedicated voice-list-only bullet), found {mentions}. \
             The generation surface must not reference \
             `podcast_voices` (codex round-4)"
        );
    }

    #[test]
    fn should_strip_voice_probe_from_generation_surface() {
        // Regression guard for NEW-05 round-3: the round-2 coercion
        // phrasings that tried to push the model THROUGH
        // `podcast_voices` for an episode-generation request must not
        // return. The voice-list-only bullet itself is preserved
        // (codex P2 round-3 review: explicit "list podcast voices"
        // requests still need a home), but the generation path no
        // longer routes through it.
        assert!(
            !PROMPT.contains("you MUST immediately follow up with `podcast_generate`"),
            "the round-2 follow-up rule must not return — round-3 \
             removes the podcast_voices coercion from the generation \
             surface"
        );
        assert!(
            !PROMPT.contains("do NOT stop after the voice list"),
            "the round-2 'do NOT stop after voice list' nudge must \
             not return — round-3 removes the voice-probe step \
             entirely from the generation path"
        );
        // The Podcast generation bullet must NOT include bare `播客`
        // or `podcast` as triggers any more — those swallowed
        // voice-list-only asks like `list podcast voices` /
        // `播客有哪些声音`. codex P2 round-3 review caught this.
        let gen_bullet = PROMPT
            .lines()
            .find(|l| l.starts_with("- Podcast generation"))
            .expect("Podcast generation bullet missing");
        assert!(
            !gen_bullet.starts_with("- Podcast generation (`播客`,"),
            "Podcast generation triggers must not lead with bare \
             `播客` — it overlaps with voice-list-only requests \
             (codex P2 round-3 review)"
        );
        assert!(
            !gen_bullet.contains("`播客`, `podcast`,"),
            "Podcast generation triggers must not include bare \
             `播客` / `podcast` — those swallow voice-list-only \
             asks (codex P2 round-3 review)"
        );
    }

    #[test]
    fn should_preserve_voice_list_only_route_for_explicit_listing() {
        // codex P2 round-3 review: removing the voice-list-only
        // bullet caused explicit "list podcast voices" /
        // `播客有哪些声音` requests to be misrouted into
        // podcast_generate. The fix is to keep the dedicated bullet
        // (with the Override carve-out) ahead of the generation
        // bullet. It is no longer load-bearing for the generation
        // flow — it just serves explicit listing intent.
        assert!(
            PROMPT.contains("Podcast voice list only"),
            "explicit voice-list requests (`list podcast voices`, \
             `播客有哪些声音`) need a dedicated route or they get \
             misrouted into podcast_generate (codex P2 round-3 \
             review)"
        );
        let voice_list_idx = PROMPT
            .find("Podcast voice list only")
            .expect("voice-list-only bullet missing");
        let generation_idx = PROMPT
            .find("Podcast generation")
            .expect("podcast generation bullet missing");
        assert!(
            voice_list_idx < generation_idx,
            "the voice-list-only bullet must appear BEFORE the \
             podcast-generation bullet so explicit listing requests \
             match first"
        );
        assert!(
            PROMPT.contains("fall through to the Podcast generation bullet below"),
            "voice-list-only bullet must explicitly fall through to \
             the Podcast generation bullet when the message asks to \
             make/generate a podcast (codex P2 round-3 carve-out)"
        );
    }

    #[test]
    fn should_instruct_model_to_draft_script_inline() {
        // codex P1 round-3 review: the original round-3 prompt told
        // the model to call `podcast_generate` "with topic/duration",
        // but the tool actually requires `script` or `script_path`.
        // The prompt must now tell the model to draft a markdown
        // script inline and pass it as the `script` argument.
        assert!(
            PROMPT.contains("draft a full markdown dialogue script"),
            "prompt must tell the model to draft a markdown dialogue \
             script inline — podcast_generate requires script or \
             script_path, not topic/duration (codex P1 round-3 \
             review)"
        );
        assert!(
            PROMPT.contains("`script` argument"),
            "prompt must name the `script` argument explicitly so \
             the model knows which parameter receives the markdown \
             (codex P1 round-3 review)"
        );
        // The round-3 draft incorrectly said "with topic/duration".
        // That phrasing must not return.
        assert!(
            !PROMPT.contains("`podcast_generate` with topic/duration"),
            "the buggy 'with topic/duration' phrasing must not \
             return — podcast_generate does not accept those \
             arguments (codex P1 round-3 regression guard)"
        );
    }

    #[test]
    fn should_name_preset_voices_for_script_generation() {
        // codex P2 round-3 follow-up: podcast_generate validates the
        // `voice` token in every `[Character - voice, emotion]` line
        // against the skill's built-in preset list. If the model
        // invents voice names like `default`, `voice`, or
        // role-derived strings, synthesis fails before TTS even
        // starts. The prompt must name the concrete preset voices
        // the skill ships with so the model has safe defaults to
        // pick from.
        //
        // Skill source (mofa-podcast 0.4.5):
        //   PRESET_VOICES = ["vivian", "serena", "ryan", "aiden",
        //                    "eric", "dylan", "uncle_fu",
        //                    "ono_anna", "sohee"]
        for voice in [
            "vivian", "serena", "ryan", "aiden", "eric", "dylan", "uncle_fu", "ono_anna", "sohee",
        ] {
            assert!(
                PROMPT.contains(&format!("`{voice}`")),
                "prompt must name the `{voice}` preset voice so the \
                 model picks valid voice tokens for the script lines \
                 (codex P2 round-3 follow-up)"
            );
        }
        assert!(
            PROMPT.contains("Do NOT invent voice names")
                || PROMPT.contains("do NOT invent voice names"),
            "prompt must explicitly forbid inventing voice names — \
             arbitrary tokens like `default` or `voice` fail \
             validation in podcast_generate (codex P2 round-3 \
             follow-up)"
        );
    }

    #[test]
    fn should_allow_clone_voices_in_generated_script() {
        // codex round-3 P2 (third pass): the previous wording "voice
        // token MUST be a built-in preset" forbade clone voices like
        // `clone:yangmi`, but podcast_generate accepts them. The
        // prompt must allow callers' explicit clone voices and treat
        // presets only as the fallback when no clone voice is named.
        assert!(
            PROMPT.contains("`clone:"),
            "prompt must mention the `clone:` voice prefix so the \
             model knows clone voices are valid in scripts (codex \
             round-3 P2 third-pass review)"
        );
        // The MUST-built-in language must not return on either
        // podcast rule.
        assert!(
            !PROMPT.contains("`voice` token MUST be a built-in preset"),
            "the over-constrained 'MUST be a built-in preset' \
             wording must not return — clone voices are valid \
             inputs (codex round-3 P2 third-pass review)"
        );
        assert!(
            !PROMPT.contains("voice token MUST be a built-in preset"),
            "the over-constrained 'MUST be a built-in preset' \
             wording must not return — clone voices are valid \
             inputs (codex round-3 P2 third-pass review)"
        );
        assert!(
            !PROMPT.contains("`voice` token in each `[Character - voice, emotion]` header MUST be a built-in preset voice"),
            "the over-constrained 'MUST be a built-in preset voice' \
             phrasing must not return (codex round-3 P2 third-pass \
             review)"
        );
    }

    #[test]
    fn should_not_let_bare_generate_token_trigger_listing_override() {
        // codex P2 round-3 follow-up: the override carve-out on the
        // voice-list-only bullet originally listed bare `生成` and
        // `generate` as triggers that fall through to the
        // generation bullet. But `生成可用的播客声音列表` /
        // "generate the available voice list" is a LISTING request,
        // not an episode-generation request. The override must
        // require specific episode-generation phrasing.
        let voice_list_bullet = PROMPT
            .lines()
            .find(|l| l.starts_with("- Podcast voice list only"))
            .expect("voice-list-only bullet missing");
        // The override must NOT use bare `生成` / `generate` as
        // triggers any more.
        assert!(
            !voice_list_bullet.contains("`生成`, `make`, `generate`,"),
            "override carve-out must not list bare `生成` / \
             `generate` as triggers — they swallow listing requests \
             like `生成可用的播客声音列表` (codex P2 round-3 \
             follow-up). Current bullet: {voice_list_bullet}"
        );
        // The override must require a podcast-episode-specific phrase.
        assert!(
            voice_list_bullet.contains("`生成播客`")
                || voice_list_bullet.contains("`generate a podcast`"),
            "override carve-out must trigger on episode-specific \
             phrasing like `生成播客` or `generate a podcast`, not \
             bare verbs (codex P2 round-3 follow-up). Current \
             bullet: {voice_list_bullet}"
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
            PROMPT.contains("prefer the `news_fetch` specialist tool when it is registered"),
            "Grounding Rules must explicitly prefer news_fetch over \
             web_search/web_fetch when news_fetch is registered (codex \
             P2 follow-up to B6 fix)"
        );
        // The old conflicting phrasing — "news, current events" lumped
        // into the web_search bucket — must not return.
        assert!(
            !PROMPT.contains("(stock prices, sports scores, exchange rates, news, current events"),
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
