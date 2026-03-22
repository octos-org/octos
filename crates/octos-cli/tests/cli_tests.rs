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

#[test]
fn test_init_defaults_in_temp_dir() {
    let temp_dir = tempfile::tempdir().unwrap();

    let output = Command::new(octos_binary())
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
