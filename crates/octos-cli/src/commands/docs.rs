//! Documentation generator command.

use std::path::PathBuf;

use clap::Args;
use eyre::Result;
use octos_agent::ToolRegistry;

use super::Executable;

/// Generate documentation for tools and providers.
#[derive(Debug, Args)]
pub struct DocsCommand {
    /// Output directory (default: stdout).
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

impl Executable for DocsCommand {
    fn execute(self) -> Result<()> {
        let content = generate_docs();

        if let Some(dir) = &self.output {
            std::fs::create_dir_all(dir)?;
            let path = dir.join("TOOLS.md");
            std::fs::write(&path, &content)?;
            println!("Documentation written to {}", path.display());
        } else {
            print!("{content}");
        }
        Ok(())
    }
}

fn generate_docs() -> String {
    let mut out = String::new();

    out.push_str("# octos Documentation\n\n");

    // Tools section
    out.push_str("## Tools\n\n");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = ToolRegistry::with_builtins(&cwd);
    let mut specs = registry.specs();
    specs.sort_by(|a, b| a.name.cmp(&b.name));

    for spec in &specs {
        out.push_str(&format!("### `{}`\n\n", spec.name));
        out.push_str(&format!("{}\n\n", spec.description));
        out.push_str("**Input Schema:**\n\n```json\n");
        if let Ok(pretty) = serde_json::to_string_pretty(&spec.input_schema) {
            out.push_str(&pretty);
        }
        out.push_str("\n```\n\n---\n\n");
    }

    // Providers section
    out.push_str("## Providers\n\n");
    for (name, env_var, default_model, base_url) in providers_list() {
        out.push_str(&format!("### {name}\n\n"));
        out.push_str(&format!("- **API Key Env:** `{env_var}`\n"));
        out.push_str(&format!("- **Default Model:** `{default_model}`\n"));
        if let Some(url) = base_url {
            out.push_str(&format!("- **Base URL:** `{url}`\n"));
        }
        out.push('\n');
    }

    out
}

fn providers_list() -> Vec<(
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
)> {
    vec![
        (
            "Anthropic",
            "ANTHROPIC_API_KEY",
            "claude-sonnet-4-20250514",
            None,
        ),
        ("OpenAI", "OPENAI_API_KEY", "gpt-4o", None),
        ("Gemini", "GEMINI_API_KEY", "gemini-2.0-flash", None),
        (
            "OpenRouter",
            "OPENROUTER_API_KEY",
            "anthropic/claude-sonnet-4-20250514",
            None,
        ),
        (
            "DeepSeek",
            "DEEPSEEK_API_KEY",
            "deepseek-chat",
            Some("https://api.deepseek.com/v1"),
        ),
        (
            "Groq",
            "GROQ_API_KEY",
            "llama-3.3-70b-versatile",
            Some("https://api.groq.com/openai/v1"),
        ),
        (
            "Moonshot",
            "MOONSHOT_API_KEY",
            "kimi-k2.5",
            Some("https://api.moonshot.ai/v1"),
        ),
        (
            "DashScope",
            "DASHSCOPE_API_KEY",
            "qwen-max",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        ),
        (
            "MiniMax",
            "MINIMAX_API_KEY",
            "MiniMax-Text-01",
            Some("https://api.minimax.io/v1"),
        ),
        (
            "Zhipu",
            "ZHIPU_API_KEY",
            "glm-4-plus",
            Some("https://open.bigmodel.cn/api/paas/v4"),
        ),
        (
            "Ollama",
            "(none)",
            "llama3.2",
            Some("http://localhost:11434/v1"),
        ),
    ]
}
