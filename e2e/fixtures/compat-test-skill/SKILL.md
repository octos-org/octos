---
name: compat-test-skill
description: Summarizes a text file into a compact report. Used as a third-party compatibility harness. Triggers: summarize, summary, compat-test, harness check.
version: 1.0.0
author: octos-harness-compat
always: false
requires_env: COMPAT_SUMMARY_TOKEN
---

# Compat Test Skill

A minimal third-party skill used by the harness compatibility gate. It reads a
text file, emits a short summary, and declares the summary file as a deliverable
artifact via the documented `files_to_send` field.

This skill intentionally avoids any runtime-internal types. It uses only the
stable developer interface fields documented in `docs/app-skill-dev-guide.md`:

- `manifest.json`: `name`, `version`, `author`, `description`, `timeout_secs`,
  `requires_network`, `tools[].name`, `tools[].description`,
  `tools[].input_schema`, `tools[].env`
- `SKILL.md` frontmatter: `name`, `description`, `version`, `author`, `always`,
  `requires_env`
- Binary protocol: `./main <tool_name>` with JSON on stdin, JSON with `output`,
  `success`, `files_to_send` on stdout.

## Tools

### summarize_text

Reads a text file at `input_path`, writes a compact summary to `output_path`,
and returns `output_path` as a deliverable artifact.

**Parameters:**
- `input_path` (required): absolute path of the source text file
- `output_path` (required): absolute path to write the summary to

## Secret Handling

The skill declares `COMPAT_SUMMARY_TOKEN` via `requires_env` and `tools[].env`.
The runtime strips secret-like environment variables from plugin subprocesses
unless allowlisted in `tools[].env`. The skill NEVER writes the secret value
to stdout, stderr, or disk — only the declared env-var *name* appears in the
manifest and SKILL.md.
