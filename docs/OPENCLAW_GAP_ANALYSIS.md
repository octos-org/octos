# OpenClaw Top 10 Skills — Gap Analysis vs octos

> Based on the viral "龙虾必看！OpenClaw全网最实用的10个技能" article. Mapped against octos actual capabilities as of 2026-03-15.

## Summary

| # | OpenClaw Skill | What It Does | octos Equivalent | Coverage | Gap |
|---|---|---|---|---|---|
| 1 | **self-improving-agent** (自我迭代) | Remember errors, self-optimize over time | `EpisodeStore` + `save_memory`/`recall_memory` + hybrid BM25+vector search | 90% | Has cross-session memory and outcome tracking. Missing: explicit error→correction feedback loop |
| 2 | **gog** (Google全家桶) | Gmail, Calendar, Drive, Docs automation | `send-email` skill (SMTP only) | 10% | No Google API integration. Only outbound SMTP email |
| 3 | **tavily-search** (联网搜索) | Real-time web search via Tavily API | `web_search` with 5-provider fallback chain (Exa→DuckDuckGo→Brave→You.com→Perplexity) | 120% | **Exceeds** — multi-provider, no single API dependency |
| 4 | **summarize** (多格式总结) | Summarize URLs, PDFs, YouTube, audio | `deep_search` + `synthesize_research` + `web_fetch` + `browser` | 70% | URL/web: yes. PDF: browser rendering only. **Missing**: YouTube transcript, audio parsing |
| 5 | **github** (GitHub集成) | Issues, PRs, repo management via gh CLI | `git` tool (read-only: status/diff/log/blame) | 30% | Local git only. **Missing**: GitHub API (issues, PRs, code search, repo creation) |
| 6 | **ontology** (结构化记忆) | Structured cross-session memory graph | `save_memory`/`recall_memory` entity pages + `EpisodeStore` + hybrid search | 95% | Entity-based memory with merge-on-update. Missing: linked graph/ontology structure |
| 7 | **find-skills** (技能自动推荐) | Auto-discover and install skills from hub | `manage_skills` tool (install from GitHub URL) | 40% | Can install by URL. **Missing**: skill marketplace, auto-discovery, recommendations |
| 8 | **weather** (天气查询) | No-API-key weather lookup | `get_weather` + `get_forecast` (Open-Meteo, free) | 100% | Full parity |
| 9 | **proactive-agent** (主动规划) | Autonomous task planning and execution | `spawn` background tasks + `run_pipeline` (DynamicParallel) + `cron` jobs | 110% | **Exceeds** — background workers, multi-step DAG pipelines, per-node model selection, cron |
| 10 | **skill-vetter** (安全扫描) | Scan skill code for malware before install | Sandbox (Bwrap/macOS/Docker) + tool policies + SSRF protection + env sanitization | 60% | Strong runtime security. **Missing**: pre-install static code analysis |

## Scorecard

| Status | Count | Skills |
|--------|-------|--------|
| Exceeds (>100%) | 2 | tavily-search, proactive-agent |
| Full parity (100%) | 1 | weather |
| Strong (80-99%) | 2 | self-improving-agent, ontology |
| Partial (30-70%) | 3 | summarize, skill-vetter, find-skills |
| Weak (<30%) | 1 | github |
| Missing (0%) | 1 | gog (Google suite) |

## Priority Gaps

| Gap | Effort | Impact | Approach |
|-----|--------|--------|----------|
| **GitHub tool** | 1 day | High | App-skill wrapping `gh` CLI (issues, PRs, code search) |
| **YouTube transcript** | 0.5 day | Medium | Add `yt-dlp --write-sub` to summarize pipeline |
| **Google suite** | 3-5 days | High | MCP server with OAuth2 + Google REST APIs (Gmail, Calendar, Drive) |
| **Skill hub/marketplace** | 2-3 days | Medium | JSON registry on GitHub + `find-skills` query support |
| **Pre-install skill scanner** | 1-2 days | Low | Static analysis of skill manifests + source before installation |
| **Error feedback loop** | 1 day | Medium | Auto-inject "last failure context" from episodes into system prompt |
