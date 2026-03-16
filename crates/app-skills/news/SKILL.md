---
name: news
version: 1.0.0
author: octos
always: true
---

# News Digest Skill

You have a `news_fetch` tool that fetches raw news headlines and article content from multiple sources (Google News RSS, Hacker News API, Yahoo News, Substack, Medium). Use it to build daily news digests for the user.

## How to use

1. Call the `news_fetch` tool with the desired categories and language.
2. The tool returns raw headlines organized by category, plus deep-fetched full article content for the top stories.
3. **You** synthesize the raw data into a well-structured news digest. The tool does NOT do any summarization -- that is your job.

### Tool parameters

- `categories` (optional, array of strings): Which news categories to fetch. If empty or omitted, all categories are fetched. Available categories:
  - `politics` -- US politics
  - `world` (alias: `international`) -- International news
  - `business` (alias: `commerce`) -- Business and finance
  - `technology` (alias: `tech`) -- Technology
  - `science` -- Science
  - `entertainment` (alias: `social`) -- Entertainment and society
  - `health` -- Health
  - `sports` -- Sports

- `language` (optional, string): Output language hint, `"zh"` (Chinese, default) or `"en"` (English). This tells you what language to write the digest in; the fetched content is always in English.

### Example call

```json
{
  "categories": ["tech", "world"],
  "language": "zh"
}
```

## Synthesis instructions

When you receive the raw data from `news_fetch`, synthesize it into a digest following these rules:

1. Use the language specified (Chinese by default, English if `"en"`).
2. Start with a title: `# 每日新闻速递 YYYY-MM-DD` (or `# Daily News Digest YYYY-MM-DD` for English).
3. Group stories by category with clear headers.
4. For stories that have full article content (in the `FULL ARTICLE CONTENT` section), write detailed 2-3 sentence summaries.
5. For headline-only stories, write 1 sentence summaries based on the headline.
6. Deduplicate stories that appear across multiple sources.
7. Skip ads, navigation text, and cookie notices from the raw content.
8. Include 5-10 most important stories per category.
9. For Hacker News items, prioritize high-score stories.
10. Use markdown formatting throughout.

## Cron subscription

This skill supports daily delivery via cron. To set up a daily news digest:

- Schedule a cron job that sends a message like "fetch today's news digest" or "每日新闻" to the agent.
- The agent will call `news_fetch`, synthesize the digest, and deliver it through the configured channel (Telegram, Lark, etc.).
- Recommended schedule: daily at 8:00 AM local time.

Example cron configuration:
```json
{
  "schedule": "0 8 * * *",
  "message": "请生成今日新闻速递，包含所有分类"
}
```
