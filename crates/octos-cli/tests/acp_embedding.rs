//! Pins the ACP embedding seam: [`AcpCommand::factory`] must stay reachable,
//! and buildable, from OUTSIDE the crate.
//!
//! This is an integration test on purpose — it links `octos-cli` as a library
//! exactly as an embedder does, so it fails if `commands::acp`,
//! `SessionAgentFactory` or `factory()` ever stop being public. A unit test
//! inside the crate would keep passing after such a change and prove nothing.
//!
//! Why the seam exists: a host that cannot spawn `octos acp` as a child process
//! (iOS prohibits `exec()`) otherwise has to reimplement the agent assembly by
//! hand, and such a reimplementation is a subset that drifts — missing provider
//! fallbacks, the auth store, `keychain:` markers, MCP, plugins and skills.
//! Handing out the same factory the stdio path drives keeps embedders in step.

#![cfg(feature = "api")]

use octos_cli::commands::acp::{AcpCommand, DEFAULT_MAX_ITERATIONS, SessionAgentFactory};
use tempfile::TempDir;

/// The factory builds from a provider name alone — no key, no network.
///
/// That is the contract that makes this usable at startup: the provider chain,
/// episode store and memory are built lazily on the first `session/new`, so a
/// missing credential surfaces there rather than as a construction failure
/// before the client has even finished its handshake.
#[test]
fn should_build_a_factory_from_outside_the_crate() {
    let dir = TempDir::new().unwrap();
    let cmd = AcpCommand {
        cwd: Some(dir.path().to_path_buf()),
        data_dir: Some(dir.path().join("data")),
        provider: Some("anthropic".to_string()),
        ..Default::default()
    };

    let factory = cmd
        .factory()
        .expect("factory should build from a provider name alone");

    // Reached through the trait object, which is what an embedder holds.
    let factory: &dyn SessionAgentFactory = factory.as_ref();
    assert_eq!(
        factory.default_cwd(),
        dir.path(),
        "the factory must root sessions at the cwd it was given",
    );
}

/// `Default` has to carry the CLI's real Codex-style default. `0` is an
/// explicit sentinel meaning an unlimited interactive turn; it no longer
/// means "zero calls allowed" at the budget gate.
#[test]
fn should_default_max_iterations_to_the_cli_value() {
    assert_eq!(AcpCommand::default().max_iterations, DEFAULT_MAX_ITERATIONS);
    assert_eq!(DEFAULT_MAX_ITERATIONS, 0);
}

/// No provider anywhere is a typed error, not a panic — an embedder needs to
/// turn it into its own "set up a provider" prompt.
#[test]
fn should_error_when_no_provider_can_be_resolved() {
    let dir = TempDir::new().unwrap();
    let empty = dir.path().join("empty.json");
    std::fs::write(&empty, "{}").unwrap();

    let cmd = AcpCommand {
        cwd: Some(dir.path().to_path_buf()),
        data_dir: Some(dir.path().join("data")),
        config: Some(empty),
        ..Default::default()
    };

    // Matched rather than `expect_err`: the Ok type is a trait object, which
    // isn't `Debug`, and that bound is what `expect_err` needs.
    let err = match cmd.factory() {
        Ok(_) => panic!("an empty config names no provider"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("provider"),
        "the error has to say what is missing, got: {err}",
    );
}
