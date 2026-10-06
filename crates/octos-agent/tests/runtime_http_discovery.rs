//! Verifies that the async HTTP discovery pass re-registers HTTP-discovered
//! tools on every startup — fixes Critical #1 from the PR #1260 code review.
//!
//! Architecture (Path 2 from the PR #1346 → #1347 merge):
//!
//! Static (binary-protocol) skills register through the sync
//! `PluginLoader::load_into_with_options`. HTTP-discovery skills get their
//! catalog walked by an explicit async pass —
//! `register_http_skills_on_startup` — that the agent boot path (chat /
//! gateway / serve) invokes after the sync loader returns. The two passes
//! together produce the same registry state the old in-loader HTTP path
//! produced; splitting them just lets the cache pipeline stay sync.

use std::net::{SocketAddr, TcpListener};

use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use octos_agent::permissions::SafetyTier;
use octos_agent::plugins::{PluginLoader, activate_skill, register_http_skills_on_startup};
use octos_agent::tools::ToolRegistry;
use octos_agent::tools::robot_groups;

#[tokio::test]
async fn startup_pass_registers_http_tools_from_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tools"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name": "robot.heartbeat", "description": "ping the robot", "safety_tier": "observe"}
        ])))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let skill_dir = dir.path().join("test-robot");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let manifest = json!({
        "name": "test-robot",
        "version": "0.1.0",
        "required_safety_tier": "safe_motion",
        "tool_discovery": { "type": "http", "base_url": server.uri() }
    });
    std::fs::write(
        skill_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut registry = ToolRegistry::new();
    let plugin_dirs = vec![dir.path().to_path_buf()];
    PluginLoader::load_into_with_options(&mut registry, &plugin_dirs, &[], Default::default())
        .expect("static load_into should succeed");
    register_http_skills_on_startup(&mut registry, &plugin_dirs)
        .await
        .expect("HTTP startup pass should succeed");

    // Names are sanitized for LLM provider tool-name pattern compatibility:
    // dots become underscores. The original SPEC-V1 verb is preserved in
    // DoraToolMapping.verb_path for bridge URL dispatch.
    assert!(
        registry.get_tool("robot_heartbeat").is_some(),
        "HTTP-discovered tool missing from registry"
    );
    assert_eq!(
        robot_groups::snapshot().tier_of("robot_heartbeat"),
        Some(SafetyTier::Observe),
        "catalog safety_tier should win"
    );
}

#[tokio::test]
async fn startup_pass_falls_back_to_manifest_required_safety_tier_when_catalog_omits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tools"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name": "vendor.x.y.motion.go", "description": "go"}
        ])))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let skill_dir = dir.path().join("legacy-robot");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let manifest = json!({
        "name": "legacy-robot",
        "version": "0.1.0",
        "required_safety_tier": "safe_motion",
        "tool_discovery": { "type": "http", "base_url": server.uri() }
    });
    std::fs::write(
        skill_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut registry = ToolRegistry::new();
    let plugin_dirs = vec![dir.path().to_path_buf()];
    PluginLoader::load_into_with_options(&mut registry, &plugin_dirs, &[], Default::default())
        .unwrap();
    register_http_skills_on_startup(&mut registry, &plugin_dirs)
        .await
        .unwrap();

    // Sanitized: dots in the catalog name become underscores at registration.
    assert!(registry.get_tool("vendor_x_y_motion_go").is_some());
    assert_eq!(
        robot_groups::snapshot().tier_of("vendor_x_y_motion_go"),
        Some(SafetyTier::SafeMotion),
        "manifest required_safety_tier should be the fallback"
    );
}

#[tokio::test]
async fn startup_pass_uses_tool_overrides_when_present() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tools"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name": "robot.estop", "description": "stop"}
        ])))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let skill_dir = dir.path().join("override-robot");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let manifest = json!({
        "name": "override-robot",
        "version": "0.1.0",
        "required_safety_tier": "safe_motion",
        "tool_overrides": { "robot.estop": "emergency_override" },
        "tool_discovery": { "type": "http", "base_url": server.uri() }
    });
    std::fs::write(
        skill_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut registry = ToolRegistry::new();
    let plugin_dirs = vec![dir.path().to_path_buf()];
    PluginLoader::load_into_with_options(&mut registry, &plugin_dirs, &[], Default::default())
        .unwrap();
    register_http_skills_on_startup(&mut registry, &plugin_dirs)
        .await
        .unwrap();

    // Sanitized: tool_overrides key in the manifest uses the dotted SPEC-V1
    // verb; the resolver matches it before sanitization, then the resulting
    // tool registers under the sanitized name.
    assert!(registry.get_tool("robot_estop").is_some());
    assert_eq!(
        robot_groups::snapshot().tier_of("robot_estop"),
        Some(SafetyTier::EmergencyOverride),
        "tool_overrides should beat manifest default"
    );
}

/// Important #1 fix from PR #1260 review: `activate_skill` (install-time)
/// must use the same helper as the runtime startup pass so a freshly
/// installed skill is policy-gated immediately — registers as
/// `DoraToolBridge` AND enrols in `robot_groups`, identically to the
/// runtime-startup path.
#[tokio::test]
async fn activate_skill_registers_http_tools_and_robot_groups() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tools"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name": "install.robot.move", "description": "move arm"}
        ])))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let skill_dir = dir.path().join("install-robot");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let manifest = json!({
        "name": "install-robot",
        "version": "0.1.0",
        "required_safety_tier": "safe_motion",
        "tool_discovery": { "type": "http", "base_url": server.uri() }
    });
    std::fs::write(
        skill_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut registry = ToolRegistry::new();
    let result = activate_skill(&mut registry, &skill_dir, &[])
        .await
        .expect("activate_skill should succeed");

    // Sanitized: install path uses the same `install_http_tools_from_catalog`
    // helper, so the tool registers under the sanitized name `install_robot_move`.
    assert!(
        result
            .tool_names
            .contains(&"install_robot_move".to_string())
    );
    assert!(
        registry.get_tool("install_robot_move").is_some(),
        "install-time HTTP-discovered tool missing from registry"
    );
    assert_eq!(
        robot_groups::snapshot().tier_of("install_robot_move"),
        Some(SafetyTier::SafeMotion),
        "install path must enrol tools in robot_groups (mirrors runtime path)"
    );
}

/// PR #1260 review (Finding 2): startup-time HTTP discovery failure must
/// not register zero tools and return success — that leaves an installed
/// HTTP-backed skill silently unavailable until manual restart. The
/// startup pass must hard-fail so the operator notices and CI catches
/// the regression.
#[tokio::test]
async fn startup_pass_hard_fails_when_http_bridge_unreachable() {
    // Grab a real loopback port, then immediately drop the listener so
    // connections to it get "connection refused" — emulates a bridge that
    // crashed before the agent restarted.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr: SocketAddr = listener.local_addr().unwrap();
    drop(listener);
    let dead_base_url = format!("http://{addr}");

    let dir = tempdir().unwrap();
    let skill_dir = dir.path().join("offline-bridge-robot");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let manifest = json!({
        "name": "offline-bridge-robot",
        "version": "0.1.0",
        "required_safety_tier": "safe_motion",
        "tool_discovery": { "type": "http", "base_url": dead_base_url }
    });
    std::fs::write(
        skill_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut registry = ToolRegistry::new();
    let plugin_dirs = vec![dir.path().to_path_buf()];
    // Static load_into is a no-op here (the manifest has no static tools)
    // but it must still succeed — the HTTP path is the gate, not this one.
    PluginLoader::load_into_with_options(&mut registry, &plugin_dirs, &[], Default::default())
        .expect("sync load_into should still succeed for HTTP-only manifests");

    let err = register_http_skills_on_startup(&mut registry, &plugin_dirs)
        .await
        .expect_err("HTTP startup pass must fail when the bridge is unreachable");

    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("http")
            || msg.to_lowercase().contains("bridge")
            || msg.to_lowercase().contains("discovery"),
        "error should name the failed transport, got: {msg}"
    );
}

/// Companion to the above: a catalog endpoint that returns malformed JSON
/// must also fail-fast, not silently register zero tools.
#[tokio::test]
async fn startup_pass_hard_fails_when_catalog_returns_garbage() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tools"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not valid json {{{"))
        .mount(&server)
        .await;

    let dir = tempdir().unwrap();
    let skill_dir = dir.path().join("garbage-catalog-robot");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let manifest = json!({
        "name": "garbage-catalog-robot",
        "version": "0.1.0",
        "required_safety_tier": "safe_motion",
        "tool_discovery": { "type": "http", "base_url": server.uri() }
    });
    std::fs::write(
        skill_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut registry = ToolRegistry::new();
    let plugin_dirs = vec![dir.path().to_path_buf()];
    PluginLoader::load_into_with_options(&mut registry, &plugin_dirs, &[], Default::default())
        .expect("sync load_into should still succeed for HTTP-only manifests");

    let err = register_http_skills_on_startup(&mut registry, &plugin_dirs)
        .await
        .expect_err("HTTP startup pass must fail when catalog response cannot be parsed");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("http") || msg.contains("bridge") || msg.contains("discovery"),
        "error should name the failed transport, got: {msg}"
    );
}
