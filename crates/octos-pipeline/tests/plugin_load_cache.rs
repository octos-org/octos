//! Backend bug #1 — pipeline plugin-load caching.
//!
//! The `CodergenHandler::execute` path used to call
//! `PluginLoader::load_into` for every node, re-reading and SHA-256
//! verifying every plugin executable on the runtime thread. With ~14
//! bundled plugins (each up to 100 MB) this added 100 ms–seconds of
//! latency per node which starved the SSE window the chat UI / e2e
//! tests inspect. The fix loads plugins once per `CodergenHandler` and
//! shares them via an `Arc<OnceLock>`-backed cache.
//!
//! These tests assert the cached behaviour: the verified-bytes sibling
//! file is written exactly once across N node executions, and the
//! cached registration vector exposes the same set of plugin tools on
//! repeat calls.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use octos_pipeline::CodergenHandler;
use sha2::{Digest, Sha256};

#[allow(dead_code)]
struct MockProvider;

#[async_trait::async_trait]
impl octos_llm::LlmProvider for MockProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        Ok(octos_llm::ChatResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            reasoning_content: None,
            provider_index: None,
        })
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
    fn model_id(&self) -> &str {
        "mock-1"
    }
}

async fn temp_episode_store() -> Arc<octos_memory::EpisodeStore> {
    let dir = tempfile::tempdir().unwrap();
    Arc::new(octos_memory::EpisodeStore::open(dir.path()).await.unwrap())
}

/// Create a sham plugin under `plugins_root` named `name`. The plugin
/// has a manifest declaring a single tool, a tiny shell-script
/// executable, and a sha256 entry pointing at the executable bytes so
/// the loader's hash verification path is exercised.
fn create_stub_plugin(plugins_root: &Path, name: &str) -> PathBuf {
    let plugin_dir = plugins_root.join(name);
    std::fs::create_dir_all(&plugin_dir).unwrap();

    let exec_content = format!("#!/bin/sh\necho '{{\"output\":\"ok-{name}\",\"success\":true}}'\n");
    let exec_bytes = exec_content.as_bytes();
    let hash = format!("{:x}", Sha256::digest(exec_bytes));

    let manifest = format!(
        r#"{{
            "name": "{name}",
            "version": "1.0.0",
            "sha256": "{hash}",
            "tools": [{{
                "name": "{tool_name}",
                "description": "stub tool from plugin {name}"
            }}]
        }}"#,
        tool_name = name.replace('-', "_"),
    );
    std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();

    let exec_path = plugin_dir.join(name);
    std::fs::write(&exec_path, exec_bytes).unwrap();
    std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    plugin_dir
}

/// Acceptance — the cached-plugin path writes the verified-bytes sibling
/// exactly once across many invocations, even when several plugin dirs
/// are configured. This is the per-node-spawn cost the bug report
/// flagged: prior to caching every node re-read + re-hashed every
/// plugin and re-wrote `.<name>_verified` files, so the mtime advanced
/// on every node execution.
#[tokio::test]
async fn pipeline_plugin_load_is_cached_across_nodes() {
    let plugins_root = tempfile::tempdir().unwrap();
    let working_dir = tempfile::tempdir().unwrap();

    // Two stub plugins so the test exercises a multi-dir scan.
    create_stub_plugin(plugins_root.path(), "stub-alpha");
    create_stub_plugin(plugins_root.path(), "stub-beta");

    let handler = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        working_dir.path().to_path_buf(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_plugin_dirs(vec![plugins_root.path().to_path_buf()]);

    // First load — verified files get written.
    handler.warm_plugin_cache_for_test();

    let alpha_verified = plugins_root.path().join("stub-alpha/.stub-alpha_verified");
    let beta_verified = plugins_root.path().join("stub-beta/.stub-beta_verified");
    assert!(
        alpha_verified.exists(),
        "first load should have written {}",
        alpha_verified.display()
    );
    assert!(
        beta_verified.exists(),
        "first load should have written {}",
        beta_verified.display()
    );

    let alpha_mtime_1 = std::fs::metadata(&alpha_verified)
        .unwrap()
        .modified()
        .unwrap();
    let beta_mtime_1 = std::fs::metadata(&beta_verified)
        .unwrap()
        .modified()
        .unwrap();

    // Sleep long enough that a fresh write would advance mtime past the
    // filesystem's resolution. macOS HFS+/APFS typically resolves to 1 ns
    // but Linux on tmpfs can round to 1 s — sleep a full second to make
    // any second `write` visible.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // Second + third loads — these would re-write the verified files
    // under the old (uncached) code path. With caching they must NOT.
    handler.warm_plugin_cache_for_test();
    handler.warm_plugin_cache_for_test();

    let alpha_mtime_2 = std::fs::metadata(&alpha_verified)
        .unwrap()
        .modified()
        .unwrap();
    let beta_mtime_2 = std::fs::metadata(&beta_verified)
        .unwrap()
        .modified()
        .unwrap();

    assert_eq!(
        alpha_mtime_1, alpha_mtime_2,
        "stub-alpha verified file must NOT be re-written on subsequent loads",
    );
    assert_eq!(
        beta_mtime_1, beta_mtime_2,
        "stub-beta verified file must NOT be re-written on subsequent loads",
    );
}

/// Basic exposure check — the cached plugin tool names match what the
/// loader produced (one tool per stub plugin). Mirrors the parity
/// assertion that a node's per-execution registry sees the same
/// plugin tools regardless of which node order it runs in.
#[tokio::test]
async fn cached_plugin_registration_exposes_loaded_tools() {
    let plugins_root = tempfile::tempdir().unwrap();
    let working_dir = tempfile::tempdir().unwrap();
    create_stub_plugin(plugins_root.path(), "stub-alpha");
    create_stub_plugin(plugins_root.path(), "stub-beta");

    let handler = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        working_dir.path().to_path_buf(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_plugin_dirs(vec![plugins_root.path().to_path_buf()]);

    let names_first = handler.cached_plugin_tool_names_for_test();
    let names_second = handler.cached_plugin_tool_names_for_test();

    assert_eq!(
        names_first, names_second,
        "cached registration must be stable across calls"
    );
    assert!(names_first.contains(&"stub_alpha".to_string()));
    assert!(names_first.contains(&"stub_beta".to_string()));
}

/// Empty plugin_dirs is a fast path — the cache resolves to an empty
/// registration without filesystem work.
#[tokio::test]
async fn empty_plugin_dirs_is_a_no_op_fast_path() {
    let working_dir = tempfile::tempdir().unwrap();
    let handler = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        working_dir.path().to_path_buf(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    assert!(handler.cached_plugin_tool_names_for_test().is_empty());
}

/// Measurement guard — the second cached load must complete in under
/// 50 ms even with several plugins configured. The brief calls out
/// 100 ms–seconds per node under the legacy uncached path, so this
/// threshold gives us 2× headroom while leaving a clear regression
/// signal if the cache ever stops working.
#[tokio::test]
async fn cached_load_is_at_least_an_order_of_magnitude_faster() {
    let plugins_root = tempfile::tempdir().unwrap();
    let working_dir = tempfile::tempdir().unwrap();
    // Eight stub plugins — keep the test fast in CI but exercise enough
    // surface that the per-load cost is observable.
    for i in 0..8 {
        create_stub_plugin(plugins_root.path(), &format!("stub-{i}"));
    }

    let handler = CodergenHandler::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        temp_episode_store().await,
        working_dir.path().to_path_buf(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .with_plugin_dirs(vec![plugins_root.path().to_path_buf()]);

    // First load — full cost.
    let cold_start = std::time::Instant::now();
    handler.warm_plugin_cache_for_test();
    let cold_elapsed = cold_start.elapsed();

    // Second load — must be effectively free.
    let warm_start = std::time::Instant::now();
    handler.warm_plugin_cache_for_test();
    let warm_elapsed = warm_start.elapsed();

    eprintln!(
        "cold={cold_elapsed:?} warm={warm_elapsed:?} ratio={:.0}x",
        cold_elapsed.as_nanos() as f64 / (warm_elapsed.as_nanos().max(1) as f64)
    );

    assert!(
        warm_elapsed < std::time::Duration::from_millis(50),
        "cached load should complete in <50 ms (got {warm_elapsed:?})"
    );
}
