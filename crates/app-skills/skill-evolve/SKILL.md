# Skill Evolve

Automatic skill self-correction system. Monitors plugin tool failures and generates
improvement suggestions for SKILL.md files.

## How It Works

- An `after_tool_call` hook fires on every tool execution
- On failures from plugin tools, the hook identifies the owning skill
- An LLM analyzes the error and suggests a one-line instruction to prevent it
- Suggestions are stored in `evolutions.json` (not applied automatically)
- Use `skill_evolve list` to review pending patches
- Use `skill_evolve apply` to write patches to the skill's SKILL.md

## Commands

- `skill_evolve list` — show all pending evolution patches across skills
- `skill_evolve apply --skill <name>` — apply pending patches to SKILL.md
- `skill_evolve discard --skill <name>` — discard pending patches
