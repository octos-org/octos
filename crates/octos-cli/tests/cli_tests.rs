//! Integration tests for the octos CLI.

use std::process::Command;

/// Get the path to the octos binary.
fn octos_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // Remove test binary name
    path.pop(); // Remove deps
    path.push("octos");
    path
}

fn clear_provider_env(cmd: &mut Command) {
    for key in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GEMINI_API_KEY",
        "DEEPSEEK_API_KEY",
        "KIMI_API_KEY",
        "DASHSCOPE_API_KEY",
        "MINIMAX_API_KEY",
        "MINIMAX_CN_API_KEY",
        "ZAI_API_KEY",
    ] {
        cmd.env_remove(key);
    }
}

#[test]
fn test_help_command() {
    let output = Command::new(octos_binary())
        .arg("--help")
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("octos"));
    assert!(stdout.contains("init"));
    assert!(stdout.contains("chat"));
    assert!(stdout.contains("status"));
    assert!(stdout.contains("clean"));
    assert!(stdout.contains("completions"));
}

#[test]
fn test_version_command() {
    let output = Command::new(octos_binary())
        .arg("--version")
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("octos"));
}

#[test]
fn test_init_help() {
    let output = Command::new(octos_binary())
        .args(["init", "--help"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Initialize"));
    assert!(stdout.contains("--defaults"));
    assert!(stdout.contains("--force"));
}

#[test]
fn test_chat_help() {
    let output = Command::new(octos_binary())
        .args(["chat", "--help"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--provider"));
    assert!(stdout.contains("--model"));
    assert!(stdout.contains("--message"));
    assert!(stdout.contains("--verbose"));
}

#[test]
fn test_clean_help() {
    let output = Command::new(octos_binary())
        .args(["clean", "--help"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Clean"));
    assert!(stdout.contains("--all"));
    assert!(stdout.contains("--dry-run"));
}

#[test]
fn test_completions_help() {
    let output = Command::new(octos_binary())
        .args(["completions", "--help"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("completions"));
}

/// The canonical `model_catalog.json`, read at compile time — the same SSOT
/// `octos init` and the completion candidates (#2413) are held to.
const MODEL_CATALOG: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../model_catalog.json"
));

/// Run the binary against the compiled-in catalog, not whatever catalog a
/// developer's own `~/.octos` holds: catalog loading is disk-first, so a
/// machine that has run octos would otherwise shadow the SSOT and break the
/// comparisons below. Also clears the completion channel so a globally
/// exported var can't turn the invocation into a completion answer.
fn run_completions(args: &[&str]) -> String {
    static CALL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "octos-cli-tests-{}-{}",
        std::process::id(),
        CALL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&scratch).expect("scratch dir is created");
    let mut cmd = Command::new(octos_binary());
    cmd.args(args)
        .current_dir(&scratch)
        .env_remove("OCTOS_COMPLETE")
        .env_remove("COMPLETE");
    for home_var in ["HOME", "USERPROFILE"] {
        cmd.env(home_var, &scratch);
    }
    let output = cmd.output().expect("Failed to execute command");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success());
    let _ = std::fs::remove_dir_all(&scratch);
    stdout
}

#[test]
fn test_completions_dynamic_models_match_catalog() {
    let stdout = run_completions(&["completions", "bash", "--dynamic", "models"]);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        !lines.is_empty(),
        "model completion candidates must not be empty"
    );

    // Every candidate must be a row of the model catalog — the same SSOT
    // `octos init` offers (#2413). A hand-written list offers models the real
    // APIs no longer accept.
    let catalog: serde_json::Value =
        serde_json::from_str(MODEL_CATALOG).expect("canonical model catalog parses");
    let catalog_models: std::collections::HashSet<String> = catalog["models"]
        .as_array()
        .expect("catalog has a models array")
        .iter()
        .filter_map(|m| m["provider"].as_str())
        .filter_map(|p| p.split_once('/').map(|(_, model)| model.to_string()))
        .collect();
    for model in &lines {
        assert!(
            catalog_models.contains(*model),
            "completion offered '{model}' which is not in the model catalog"
        );
    }
    // Candidates are sorted so shells display them predictably.
    assert!(
        lines.windows(2).all(|w| w[0] < w[1]),
        "model candidates must be sorted"
    );

    // Every family-default row (the models `octos init` resolves to) must be
    // offered — the original complaint was that these were missing.
    let family_defaults: Vec<&str> = catalog["models"]
        .as_array()
        .expect("catalog has a models array")
        .iter()
        .filter(|m| m["default"].as_bool().unwrap_or(false))
        .filter_map(|m| m["provider"].as_str())
        .filter_map(|p| p.split_once('/').map(|(_, model)| model))
        .collect();
    assert!(
        !family_defaults.is_empty(),
        "catalog declares family defaults"
    );
    for default in family_defaults {
        assert!(
            lines.contains(&default),
            "catalog family default '{default}' must be offered"
        );
    }
}

#[test]
fn test_completions_dynamic_providers_match_registry() {
    let stdout = run_completions(&["completions", "bash", "--dynamic", "providers"]);
    let lines: Vec<&str> = stdout.lines().collect();

    // The candidates must be the provider registry's canonical families — the
    // same names `config.llm.provider` accepts (#2413) — not a drifting
    // hand-written subset.
    let mut expected: Vec<&str> = octos_llm::registry::all_entries()
        .iter()
        .map(|entry| entry.name)
        .collect();
    expected.sort_unstable();
    assert_eq!(
        lines, expected,
        "provider candidates must mirror the registry"
    );
}

#[test]
fn test_completions_env_channel_wiring() {
    // With OCTOS_COMPLETE set the binary answers the completion request —
    // the registration script the shell sources — and exits 0 (#2413).
    let scratch = std::env::temp_dir().join(format!("octos-cli-tests-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch dir is created");
    let answered = Command::new(octos_binary())
        .env("OCTOS_COMPLETE", "bash")
        .current_dir(&scratch)
        .output()
        .expect("Failed to execute command");
    assert!(answered.status.success());
    let stdout = String::from_utf8_lossy(&answered.stdout);
    assert!(
        stdout.contains("_clap_complete_octos"),
        "the binary must answer the completion channel with the registration script"
    );

    // The channel is namespaced: a generic COMPLETE exported for some other
    // tool must not turn octos invocations into completion answers, and the
    // empty value keeps the documented off switch.
    let unaffected = Command::new(octos_binary())
        .env("COMPLETE", "bash")
        .arg("--version")
        .output()
        .expect("Failed to execute command");
    assert!(unaffected.status.success());
    let stdout = String::from_utf8_lossy(&unaffected.stdout);
    assert!(
        !stdout.contains("_clap_complete"),
        "another tool's COMPLETE var must not hijack octos"
    );
    assert!(
        stdout.contains("octos"),
        "--version must still print a version"
    );

    let disabled = Command::new(octos_binary())
        .env("OCTOS_COMPLETE", "")
        .arg("--version")
        .output()
        .expect("Failed to execute command");
    assert!(disabled.status.success());
    let stdout = String::from_utf8_lossy(&disabled.stdout);
    assert!(
        !stdout.contains("_clap_complete"),
        "an empty OCTOS_COMPLETE must keep the channel off"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn test_completions_bash() {
    let output = Command::new(octos_binary())
        .args(["completions", "bash"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Bash completions should contain function definitions
    assert!(stdout.contains("_octos"));
}

#[test]
fn test_completions_zsh() {
    let output = Command::new(octos_binary())
        .args(["completions", "zsh"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Zsh completions should contain compdef
    assert!(stdout.contains("#compdef"));
}

#[test]
fn test_completions_fish() {
    let output = Command::new(octos_binary())
        .args(["completions", "fish"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Fish completions should contain complete command
    assert!(stdout.contains("complete"));
}

/// #2415 — the auth help must not promise the macOS Keychain on every
/// platform: each secret-store subcommand states the per-platform behavior
/// (macOS Keychain, Linux file store, Windows unsupported).
#[test]
fn test_auth_help_names_platform_secret_store() {
    for args in [
        ["auth", "set-key"],
        ["auth", "remove-key"],
        ["auth", "unlock"],
    ] {
        let output = Command::new(octos_binary())
            .args(args)
            .arg("--help")
            .output()
            .expect("Failed to execute command");

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("macOS") && stdout.contains("Linux") && stdout.contains("Windows"),
            "`{}` help must state per-platform secret-store behavior: {stdout}",
            args.join(" ")
        );
        if args[1] != "unlock" {
            // The Linux store path is the load-bearing fact of the audit.
            assert!(
                stdout.contains("~/.octos/secrets"),
                "`{}` help must name the Linux store path: {stdout}",
                args.join(" ")
            );
        }
    }
}

/// #2415 — `octos status` surfaces the active secret-store backend so users
/// can see where `auth set-key` writes on this platform (the same name the
/// `octos auth keys` header prints).
#[test]
fn test_status_reports_secret_store_backend() {
    let temp_dir = tempfile::tempdir().unwrap();

    let output = Command::new(octos_binary())
        .args(["status", "--cwd"])
        .arg(temp_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Secret store:"),
        "status must name the active secret store: {stdout}"
    );
    #[cfg(target_os = "macos")]
    assert!(stdout.contains("macos-keychain"), "{stdout}");
    #[cfg(target_os = "linux")]
    assert!(stdout.contains("linux-file"), "{stdout}");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    assert!(stdout.contains("unsupported"), "{stdout}");
}

#[test]
fn test_init_defaults_in_temp_dir() {
    let temp_dir = tempfile::tempdir().unwrap();

    let mut cmd = Command::new(octos_binary());
    clear_provider_env(&mut cmd);
    let output = cmd
        .env("ANTHROPIC_API_KEY", "test-ant-key")
        .args(["init", "--defaults", "--cwd"])
        .arg(temp_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());

    // Check config file was created
    let config_path = temp_dir.path().join(".octos").join("config.json");
    assert!(config_path.exists());

    // Check config content
    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("anthropic"));
    assert!(content.contains("claude-sonnet-4-20250514"));
}

#[test]
fn test_init_defaults_uses_octos_home_when_cwd_not_provided() {
    let temp_dir = tempfile::tempdir().unwrap();
    let octos_home = temp_dir.path().join("custom-home");
    let unrelated_cwd = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&unrelated_cwd).unwrap();

    let mut cmd = Command::new(octos_binary());
    clear_provider_env(&mut cmd);
    let output = cmd
        .env("OPENAI_API_KEY", "test-openai-key")
        .env("OCTOS_HOME", &octos_home)
        .current_dir(&unrelated_cwd)
        .args(["init", "--defaults"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());

    let home_config = octos_home.join("config.json");
    assert!(
        home_config.exists(),
        "expected init to write config into OCTOS_HOME"
    );
    assert!(
        !unrelated_cwd.join(".octos").join("config.json").exists(),
        "init should not create a separate cwd/.octos config when OCTOS_HOME is set"
    );

    let content = std::fs::read_to_string(&home_config).unwrap();
    assert!(content.contains("openai"));
    assert!(content.contains("gpt-4.1-mini"));
}

#[test]
fn test_init_defaults_refuses_to_overwrite_existing_config() {
    let temp_dir = tempfile::tempdir().unwrap();
    let octos_dir = temp_dir.path().join(".octos");
    std::fs::create_dir_all(&octos_dir).unwrap();
    let config_path = octos_dir.join("config.json");
    let original = r#"{"provider":"sentinel","model":"keep-me"}"#;
    std::fs::write(&config_path, original).unwrap();

    let mut cmd = Command::new(octos_binary());
    clear_provider_env(&mut cmd);
    let output = cmd
        .env("OPENAI_API_KEY", "test-openai-key")
        .args(["init", "--defaults", "--cwd"])
        .arg(temp_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Config already exists"));
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
}

#[test]
fn test_init_defaults_force_overwrites_existing_config() {
    let temp_dir = tempfile::tempdir().unwrap();
    let octos_dir = temp_dir.path().join(".octos");
    std::fs::create_dir_all(&octos_dir).unwrap();
    let config_path = octos_dir.join("config.json");
    std::fs::write(
        &config_path,
        r#"{"provider":"sentinel","model":"replace-me"}"#,
    )
    .unwrap();

    let mut cmd = Command::new(octos_binary());
    clear_provider_env(&mut cmd);
    let output = cmd
        .env("OPENAI_API_KEY", "test-openai-key")
        .args(["init", "--defaults", "--force", "--cwd"])
        .arg(temp_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("openai"));
    assert!(content.contains("gpt-4.1-mini"));
    assert!(!content.contains("sentinel"));
}

#[test]
fn test_clean_no_octos_dir() {
    let temp_dir = tempfile::tempdir().unwrap();

    let output = Command::new(octos_binary())
        .args(["clean", "--cwd"])
        .arg(temp_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No .octos directory"));
}

#[test]
fn test_clean_empty_octos_dir() {
    let temp_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp_dir.path().join(".octos")).unwrap();

    let output = Command::new(octos_binary())
        .args(["clean", "--cwd"])
        .arg(temp_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Nothing to clean"));
}

#[test]
fn test_clean_dry_run_with_all() {
    let temp_dir = tempfile::tempdir().unwrap();
    let octos_dir = temp_dir.path().join(".octos");
    std::fs::create_dir_all(&octos_dir).unwrap();
    std::fs::write(octos_dir.join("episodes.redb"), "fake-db").unwrap();

    let output = Command::new(octos_binary())
        .args(["clean", "--all", "--dry-run", "--cwd"])
        .arg(temp_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Would remove"));
    assert!(stdout.contains("Dry run"));

    // File should still exist
    assert!(octos_dir.join("episodes.redb").exists());
}

// ── Skill system tests ──────────────────────────────────────────────

#[test]
fn test_skills_help() {
    let output = Command::new(octos_binary())
        .args(["skills", "--help"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("list"));
    assert!(stdout.contains("install"));
    assert!(stdout.contains("remove"));
    assert!(stdout.contains("search"));
}

#[test]
fn test_skills_list() {
    let output = Command::new(octos_binary())
        .args(["skills", "list"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Built-in skills should always be present
    assert!(
        stdout.contains("cron") || stdout.contains("skill-store") || stdout.contains("Installed"),
        "skills list should show installed or built-in skills"
    );
}

/// Search the octos-hub registry for mofa skills.
#[test]
#[ignore] // Requires network access to GitHub
fn test_skills_search_registry() {
    let output = Command::new(octos_binary())
        .args(["skills", "search", "mofa"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("mofa-skills"),
        "registry should contain mofa-skills package"
    );
    assert!(
        stdout.contains("mofa-org/mofa-skills"),
        "should show install command"
    );
}

/// Search registry for a non-existent skill.
#[test]
#[ignore] // Requires network access to GitHub
fn test_skills_search_no_results() {
    let output = Command::new(octos_binary())
        .args(["skills", "search", "xyznonexistent99"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No matching") || stdout.is_empty() || !stdout.contains("Install:"),
        "should not find nonexistent skills"
    );
}

/// Install a skill from GitHub, verify it appears in list, then remove it.
#[test]
#[ignore] // Requires network access to GitHub + git
fn test_skills_install_and_remove() {
    let skill_name = "mofa-cards";
    let repo = "mofa-org/mofa-skills/mofa-cards";

    // Remove first in case it's already installed
    let _ = Command::new(octos_binary())
        .args(["skills", "remove", skill_name])
        .output();

    // Install
    let install_output = Command::new(octos_binary())
        .args(["skills", "install", repo])
        .output()
        .expect("Failed to execute install");

    assert!(
        install_output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install_output.stderr)
    );
    let stdout = String::from_utf8_lossy(&install_output.stdout);
    assert!(stdout.contains("Installed"), "should confirm installation");

    // Verify it shows in list
    let list_output = Command::new(octos_binary())
        .args(["skills", "list"])
        .output()
        .expect("Failed to execute list");

    assert!(list_output.status.success());
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_stdout.contains(skill_name),
        "installed skill should appear in list"
    );

    // Remove
    let remove_output = Command::new(octos_binary())
        .args(["skills", "remove", skill_name])
        .output()
        .expect("Failed to execute remove");

    assert!(remove_output.status.success());
    let remove_stdout = String::from_utf8_lossy(&remove_output.stdout);
    assert!(remove_stdout.contains("Removed"), "should confirm removal");

    // Verify it's gone from list
    let list_after = Command::new(octos_binary())
        .args(["skills", "list"])
        .output()
        .expect("Failed to execute list");

    let list_after_stdout = String::from_utf8_lossy(&list_after.stdout);
    assert!(
        !list_after_stdout.contains(&format!("  {skill_name} ")),
        "removed skill should not appear in list"
    );
}

#[test]
fn test_clean_all_removes_redb() {
    let temp_dir = tempfile::tempdir().unwrap();
    let octos_dir = temp_dir.path().join(".octos");
    std::fs::create_dir_all(&octos_dir).unwrap();
    std::fs::write(octos_dir.join("episodes.redb"), "fake-db").unwrap();

    let output = Command::new(octos_binary())
        .args(["clean", "--all", "--cwd"])
        .arg(temp_dir.path())
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Cleaned"));

    // Database file should be deleted
    assert!(!octos_dir.join("episodes.redb").exists());
}
