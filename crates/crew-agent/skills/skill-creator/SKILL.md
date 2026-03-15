---
name: skill-creator
description: Create custom skill packages with instructions, tools, and assets.
version: 1.0.0
author: hagency
---

# Skill Creator

Create skill packages that extend the agent with new knowledge and tools.

## Package Structure

A skill package is a directory with at least a `SKILL.md` file:

```
my-skill/
  SKILL.md            # Required: agent instructions + frontmatter
  manifest.json       # Optional: declares tool executables
  Cargo.toml          # Optional: Rust crate (if tool is written in Rust)
  src/main.rs         # Optional: tool source code
  package.json        # Optional: Node.js dependencies
  scripts/            # Optional: helper scripts
  references/         # Optional: reference docs
```

## SKILL.md Format

```markdown
---
name: my-skill
description: Brief description of what this skill does
version: 1.0.0
author: your-name
always: false
requires_bins: docker,kubectl
requires_env: GITHUB_TOKEN
---

# Skill Title

Instructions for the agent on how to use this skill.
Include examples, tool usage patterns, and best practices.
```

## Frontmatter Fields

| Field | Required | Description |
|---|---|---|
| `name` | Yes | Skill identifier (lowercase, hyphens) |
| `description` | Yes | One-line description (shown in skill index) |
| `version` | No | Semver version (e.g. `1.0.0`) |
| `author` | No | Author name or org |
| `always` | No | `true` to auto-load in every prompt (default: false) |
| `requires_bins` | No | Comma-separated binaries that must be on PATH |
| `requires_env` | No | Comma-separated env vars that must be set |

## Adding Tools

To provide executable tools that the agent can call, add a `manifest.json`:

```json
{
  "name": "my-skill",
  "version": "1.0.0",
  "tools": [
    {
      "name": "my_tool",
      "description": "What this tool does (shown to the LLM)",
      "input_schema": {
        "type": "object",
        "properties": {
          "query": {"type": "string", "description": "Search query"},
          "limit": {"type": "integer", "description": "Max results", "default": 10}
        },
        "required": ["query"]
      }
    }
  ]
}
```

The tool executable receives JSON on stdin and must output JSON on stdout:

```
stdin:  {"query": "rust async", "limit": 5}
stdout: {"output": "Results here...", "success": true}
```

### Tool executable resolution

The PluginLoader looks for executables in this order:
1. `<skill-dir>/main` — pre-built binary (downloaded from registry or built locally)
2. `<skill-dir>/<skill-name>` — named binary
3. `<skill-dir>/index.js` — Node.js script (run via `node`)

### Writing tools in Rust

Add a `Cargo.toml` and `src/main.rs`. During `crew skills install`, the system will:
1. Check the hagency skill registry for a pre-built binary (with SHA-256 verification)
2. Fall back to `cargo build --release` if no binary is available

```rust
// src/main.rs
use serde::{Deserialize, Serialize};
use std::io::Read;

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize { 10 }

#[derive(Serialize)]
struct Output {
    output: String,
    success: bool,
}

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let args: Input = serde_json::from_str(&input).unwrap();

    // Your tool logic here
    let result = format!("Searched for '{}', limit {}", args.query, args.limit);

    let output = Output { output: result, success: true };
    println!("{}", serde_json::to_string(&output).unwrap());
}
```

### Writing tools in Node.js

Add a `package.json` and `index.js`. During install, `npm install` runs automatically.

```javascript
// index.js
const input = JSON.parse(require('fs').readFileSync('/dev/stdin', 'utf8'));
const result = { output: `Processed: ${input.query}`, success: true };
console.log(JSON.stringify(result));
```

## Publishing to the Registry

1. Push your skill to a GitHub repo
2. Submit to the [hagency skill registry](https://github.com/hagency-org/skill-registry)
3. The registry team audits, builds binaries, and publishes

## Multi-Skill Repos

A single repo can contain multiple skills as top-level directories:

```
my-skills-repo/
  skill-a/
    SKILL.md
  skill-b/
    SKILL.md
    manifest.json
    src/main.rs
  shared-lib/          # Shared deps auto-detected
    ...
```

Install all: `crew skills install user/my-skills-repo`
Install one: `crew skills install user/my-skills-repo/skill-a`

## Loading Behavior

- Skills with `always: true` are included in every system prompt
- Other skills appear in the skill index (XML summary)
- The agent can read any skill on demand via `read_file`
- Skills providing tools (manifest.json) are auto-discovered by the PluginLoader

## Best Practices

1. Keep SKILL.md concise (under 200 lines)
2. Include concrete examples, not abstract theory
3. Use `requires_bins` to gate skills needing external tools
4. Set `always: true` sparingly (adds to every prompt)
5. For tools: keep them standalone with no LLM dependency — let the agent do the reasoning
6. Include version in frontmatter for update tracking
