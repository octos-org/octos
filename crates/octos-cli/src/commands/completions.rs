//! Shell completions command.

use std::io;

use clap::{Args, CommandFactory};
use clap_complete::{Shell, generate};
use eyre::Result;

use super::{Args as CliArgs, Executable};

/// Generate shell completions for octos CLI.
#[derive(Debug, Args)]
pub struct CompletionsCommand {
    /// Shell to generate completions for.
    #[arg(value_enum)]
    pub shell: Shell,

    /// Print dynamic completions for a category instead of static script.
    #[arg(long)]
    pub dynamic: Option<DynamicCategory>,
}

/// Categories available for dynamic completion.
#[derive(Debug, Clone, clap::ValueEnum)]
pub enum DynamicCategory {
    /// Known model names.
    Models,
    /// Known provider names.
    Providers,
    /// Existing session IDs.
    Sessions,
    /// Installed skills.
    Skills,
}

impl Executable for CompletionsCommand {
    fn execute(self) -> Result<()> {
        if let Some(category) = self.dynamic {
            print_dynamic(category);
        } else {
            let mut cmd = CliArgs::command();
            generate(self.shell, &mut cmd, "octos", &mut io::stdout());
        }
        Ok(())
    }
}

fn print_dynamic(category: DynamicCategory) {
    let items: Vec<&str> = match category {
        DynamicCategory::Models => vec![
            "claude-sonnet-4-20250514",
            "claude-opus-4-20250514",
            "gpt-4o",
            "gpt-4o-mini",
            "o3-mini",
            "gemini-2.0-flash",
            "deepseek-chat",
            "deepseek-reasoner",
            "llama-3.3-70b-versatile",
            "kimi-k2.5",
            "qwen-max",
            "glm-4-plus",
        ],
        DynamicCategory::Providers => vec![
            "anthropic",
            "openai",
            "gemini",
            "openrouter",
            "deepseek",
            "groq",
            "moonshot",
            "dashscope",
            "minimax",
            "zhipu",
            "ollama",
            "vllm",
        ],
        DynamicCategory::Sessions => {
            print_session_names();
            return;
        }
        DynamicCategory::Skills => {
            print_skill_names();
            return;
        }
    };
    for item in items {
        println!("{item}");
    }
}

fn print_session_names() {
    let cwd = std::env::current_dir().unwrap_or_default();
    let sessions_dir = cwd.join(".octos").join("sessions");
    if let Ok(entries) = std::fs::read_dir(sessions_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.path().file_stem().and_then(|n| n.to_str()) {
                println!("{name}");
            }
        }
    }
}

fn print_skill_names() {
    let cwd = std::env::current_dir().unwrap_or_default();
    let skills_dir = cwd.join(".octos").join("skills");
    if let Ok(entries) = std::fs::read_dir(skills_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.path().file_stem().and_then(|n| n.to_str()) {
                println!("{name}");
            }
        }
    }
}
