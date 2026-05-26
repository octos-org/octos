//! Verifies that PluginLoader::load_into_with_options re-registers HTTP-discovered
//! tools on every startup — fixes Critical #1 from the PR #1260 code review.

use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use octos_agent::permissions::SafetyTier;
use octos_agent::plugins::{PluginLoader, activate_skill};
use octos_agent::tools::ToolRegistry;
use octos_agent::tools::robot_groups;

#[tokio::test]
async fn load_into_registers_http_tools_from_catalog() {
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
    PluginLoader::load_into_with_options(
        &mut registry,
        &[dir.path().to_path_buf()],
        &[],
        Default::default(),
    )
    .await
    .expect("load_into should succeed");

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
async fn load_into_falls_back_to_manifest_required_safety_tier_when_catalog_omits() {
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
    PluginLoader::load_into_with_options(
        &mut registry,
        &[dir.path().to_path_buf()],
        &[],
        Default::default(),
    )
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
async fn load_into_uses_tool_overrides_when_present() {
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
    PluginLoader::load_into_with_options(
        &mut registry,
        &[dir.path().to_path_buf()],
        &[],
        Default::default(),
    )
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
/// must use the same helper as `load_into_with_options` so a freshly
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
