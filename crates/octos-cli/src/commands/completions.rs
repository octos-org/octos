//! Shell completions command.

use std::collections::BTreeMap;
use std::io;

use clap::{Args, CommandFactory};
use clap_complete::{Shell, generate};
use eyre::Result;

use super::init::load_catalog_models;
use super::{Args as CliArgs, Executable};

/// Generate shell completions for octos CLI.
///
/// The printed scripts are static (flags and subcommands only). For
/// completions that call back into `octos` as you type, source
/// `OCTOS_COMPLETE=<shell> octos` instead — see the book's completions section.
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
    /// Model names from the model catalog.
    Models,
    /// Provider family names from the registry.
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
    match category {
        DynamicCategory::Models => {
            for model in model_names() {
                println!("{model}");
            }
        }
        DynamicCategory::Providers => {
            for provider in provider_names() {
                println!("{provider}");
            }
        }
        DynamicCategory::Sessions => print_session_names(),
        DynamicCategory::Skills => print_skill_names(),
    }
}

/// Every model name in the model catalog — the same SSOT `octos init` reads —
/// sorted so shells display candidates predictably.
fn model_names() -> Vec<String> {
    model_names_from(load_catalog_models())
}

/// The candidate list for one parsed catalog: the model halves of every
/// `family/model` row, sorted and deduped (popular names appear under several
/// families).
fn model_names_from(catalog: BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut models: Vec<String> = catalog.into_values().flatten().collect();
    models.sort();
    models.dedup();
    models
}

/// The registry's canonical provider families — the names `config.llm.provider`
/// accepts — sorted so shells display candidates predictably.
fn provider_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = octos_llm::registry::all_entries()
        .iter()
        .map(|entry| entry.name)
        .collect();
    names.sort_unstable();
    names
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Candidate shaping: sorted and deduped, so a name that appears under
    /// several families is offered once. (The SSOT pin itself lives in the
    /// integration suite, which runs the real binary against the embedded
    /// catalog with a scrubbed HOME.)
    #[test]
    fn candidate_model_names_are_sorted_and_deduped_across_families() {
        let catalog = BTreeMap::from([
            (
                "b-family".to_string(),
                vec!["m2".to_string(), "m1".to_string()],
            ),
            (
                "a-family".to_string(),
                vec!["m1".to_string(), "m0".to_string()],
            ),
        ]);
        assert_eq!(model_names_from(catalog), vec!["m0", "m1", "m2"]);
    }

    /// Provider candidates are the registry's canonical family names — no
    /// aliases (those resolve via lookup anyway), no hand-written subset.
    #[test]
    fn dynamic_provider_names_are_registry_families() {
        let mut expected: Vec<&str> = octos_llm::registry::all_entries()
            .iter()
            .map(|entry| entry.name)
            .collect();
        expected.sort_unstable();
        assert_eq!(
            provider_names(),
            expected,
            "candidates must be exactly the registered families, sorted"
        );
        assert!(expected.contains(&"anthropic"));
    }
}
