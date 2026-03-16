//! Deep research integration tests for the browser tool.
//!
//! These tests exercise real headless Chrome against live websites.
//! Run with: `cargo test -p octos-agent -- --ignored browser_research`
//!
//! Requirements:
//! - Chrome/Chromium installed on the system
//! - Network access

use octos_agent::Tool;
use octos_agent::tools::browser::BrowserTool;
use serde_json::json;

/// Helper: execute a browser action and return (output, success)
async fn browser_action(tool: &BrowserTool, args: serde_json::Value) -> (String, bool) {
    let result = tool.execute(&args).await.expect("tool execution failed");
    (result.output, result.success)
}

/// Test basic browser session lifecycle: navigate, extract, screenshot, close.
#[tokio::test]
#[ignore]
async fn test_browser_session_lifecycle() {
    let tool = BrowserTool::new();

    // 1. Navigate to a stable page
    let (output, success) = browser_action(
        &tool,
        json!({ "action": "navigate", "url": "https://example.com" }),
    )
    .await;
    assert!(success, "navigate failed: {output}");
    assert!(output.contains("example.com"));

    // 2. Extract text — example.com has very predictable content
    let (text, success) = browser_action(&tool, json!({ "action": "get_text" })).await;
    assert!(success, "get_text failed: {text}");
    assert!(
        text.contains("Example Domain"),
        "Expected 'Example Domain' in text: {text}"
    );

    // 3. Extract HTML
    let (html, success) = browser_action(&tool, json!({ "action": "get_html" })).await;
    assert!(success, "get_html failed: {html}");
    assert!(html.contains("<html"), "Expected HTML tag: {html}");
    assert!(
        html.contains("Example Domain"),
        "Expected 'Example Domain' in HTML"
    );

    // 4. Evaluate JavaScript
    let (result, success) = browser_action(
        &tool,
        json!({ "action": "evaluate", "expression": "document.title" }),
    )
    .await;
    assert!(success, "evaluate failed: {result}");
    assert!(
        result.contains("Example Domain"),
        "Expected 'Example Domain' in title: {result}"
    );

    // 5. Get links
    let (links, success) = browser_action(&tool, json!({ "action": "get_links" })).await;
    assert!(success, "get_links failed: {links}");
    assert!(
        links.contains("Found") && links.contains("links"),
        "Expected link count: {links}"
    );

    // 6. Screenshot
    let (screenshot, success) = browser_action(&tool, json!({ "action": "screenshot" })).await;
    assert!(success, "screenshot failed: {screenshot}");
    assert!(
        screenshot.contains("Screenshot saved to"),
        "Expected screenshot path: {screenshot}"
    );
    // Verify the file exists and has content
    let path = screenshot.trim_start_matches("Screenshot saved to ");
    let metadata = tokio::fs::metadata(path)
        .await
        .expect("screenshot file missing");
    assert!(
        metadata.len() > 100,
        "Screenshot file too small: {} bytes",
        metadata.len()
    );
    // Clean up
    let _ = tokio::fs::remove_file(path).await;

    // 7. Close
    let (output, success) = browser_action(&tool, json!({ "action": "close" })).await;
    assert!(success, "close failed: {output}");
}

/// Deep research test: compare two Rust crates by visiting docs.rs (server-rendered).
/// Exercises: multi-page navigation, text extraction, JS evaluation, link following.
#[tokio::test]
#[ignore]
async fn test_deep_research_rust_crate_comparison() {
    let tool = BrowserTool::new();

    // --- Research crate 1: serde on docs.rs (server-rendered, no SPA issues) ---
    let (output, success) = browser_action(
        &tool,
        json!({ "action": "navigate", "url": "https://docs.rs/serde/latest/serde/" }),
    )
    .await;
    assert!(success, "navigate to docs.rs/serde failed: {output}");

    let (serde_text, success) = browser_action(&tool, json!({ "action": "get_text" })).await;
    assert!(success, "get_text serde failed: {serde_text}");
    let serde_lower = serde_text.to_lowercase();
    assert!(
        serde_lower.contains("serde"),
        "Expected 'serde' in page text"
    );
    // serde is a serialization framework
    assert!(
        serde_lower.contains("serial") || serde_lower.contains("deserializ"),
        "Expected serialization-related content for serde"
    );

    // Extract structured metadata via JS
    let (serde_meta, success) = browser_action(
        &tool,
        json!({
            "action": "evaluate",
            "expression": "JSON.stringify({ title: document.title, url: window.location.href })"
        }),
    )
    .await;
    assert!(success, "evaluate serde meta failed: {serde_meta}");
    assert!(
        serde_meta.contains("serde"),
        "Expected 'serde' in metadata: {serde_meta}"
    );

    // Get documentation links
    let (serde_links, success) = browser_action(&tool, json!({ "action": "get_links" })).await;
    assert!(success, "get_links serde failed: {serde_links}");
    assert!(
        serde_links.contains("Found"),
        "Expected links found: {serde_links}"
    );

    // Find elements — look for module/struct documentation links
    let (elements, success) =
        browser_action(&tool, json!({ "action": "find_elements", "selector": "a" })).await;
    assert!(success, "find_elements serde failed: {elements}");
    assert!(
        elements.contains("Found"),
        "Expected elements found: {elements}"
    );

    // --- Research crate 2: tokio on docs.rs ---
    let (output, success) = browser_action(
        &tool,
        json!({ "action": "navigate", "url": "https://docs.rs/tokio/latest/tokio/" }),
    )
    .await;
    assert!(success, "navigate to docs.rs/tokio failed: {output}");

    let (tokio_text, success) = browser_action(&tool, json!({ "action": "get_text" })).await;
    assert!(success, "get_text tokio failed: {tokio_text}");
    let tokio_lower = tokio_text.to_lowercase();
    assert!(
        tokio_lower.contains("tokio"),
        "Expected 'tokio' in page text"
    );
    // tokio is an async runtime
    assert!(
        tokio_lower.contains("async") || tokio_lower.contains("runtime"),
        "Expected async/runtime content for tokio"
    );

    // --- Cross-reference: both crates should be distinct ---
    assert_ne!(
        serde_text, tokio_text,
        "Two different crate pages should have different content"
    );

    // Navigate to tokio's task module docs (link following)
    let (output, success) = browser_action(
        &tool,
        json!({ "action": "navigate", "url": "https://docs.rs/tokio/latest/tokio/task/index.html" }),
    )
    .await;
    assert!(success, "navigate to tokio/task failed: {output}");

    let (task_text, success) = browser_action(&tool, json!({ "action": "get_text" })).await;
    assert!(success, "get_text tokio/task failed: {task_text}");
    assert!(
        task_text.to_lowercase().contains("task") || task_text.to_lowercase().contains("spawn"),
        "Expected task-related content: {task_text}"
    );

    // Screenshot the documentation page
    let (screenshot, success) = browser_action(&tool, json!({ "action": "screenshot" })).await;
    assert!(success, "screenshot failed: {screenshot}");
    assert!(screenshot.contains("Screenshot saved to"));
    let path = screenshot.trim_start_matches("Screenshot saved to ");
    let _ = tokio::fs::remove_file(path).await;

    // Clean up
    let (_, success) = browser_action(&tool, json!({ "action": "close" })).await;
    assert!(success, "close failed");
}

/// Deep research test: verify facts across multiple sources.
/// Exercises: cross-source verification, content comparison.
#[tokio::test]
#[ignore]
async fn test_deep_research_multi_source_verification() {
    let tool = BrowserTool::new();

    // --- Source 1: Wikipedia ---
    let (output, success) = browser_action(
        &tool,
        json!({ "action": "navigate", "url": "https://en.wikipedia.org/wiki/Rust_(programming_language)" }),
    )
    .await;
    assert!(success, "navigate to Wikipedia failed: {output}");

    let (wiki_text, success) = browser_action(&tool, json!({ "action": "get_text" })).await;
    assert!(success, "get_text Wikipedia failed: {wiki_text}");

    // Extract key facts from Wikipedia
    let wiki_lower = wiki_text.to_lowercase();
    let has_graydon = wiki_lower.contains("graydon");
    let has_mozilla = wiki_lower.contains("mozilla");
    let has_systems =
        wiki_lower.contains("systems programming") || wiki_lower.contains("system programming");
    let has_memory_safety =
        wiki_lower.contains("memory safety") || wiki_lower.contains("memory-safe");

    assert!(
        has_graydon || has_mozilla,
        "Wikipedia should mention Graydon Hoare or Mozilla"
    );
    assert!(
        has_systems || has_memory_safety,
        "Wikipedia should mention systems programming or memory safety"
    );

    // Get references/links
    let (wiki_links, success) = browser_action(&tool, json!({ "action": "get_links" })).await;
    assert!(success, "get_links Wikipedia failed: {wiki_links}");
    assert!(
        wiki_links.contains("rust-lang.org") || wiki_links.contains("github.com"),
        "Wikipedia should link to rust-lang.org or github: {wiki_links}"
    );

    // --- Source 2: Official Rust website ---
    let (output, success) = browser_action(
        &tool,
        json!({ "action": "navigate", "url": "https://www.rust-lang.org/" }),
    )
    .await;
    assert!(success, "navigate to rust-lang.org failed: {output}");

    let (official_text, success) = browser_action(&tool, json!({ "action": "get_text" })).await;
    assert!(success, "get_text rust-lang.org failed: {official_text}");

    let official_lower = official_text.to_lowercase();
    // Official site should mention reliability, performance, or productivity
    assert!(
        official_lower.contains("reliable")
            || official_lower.contains("performance")
            || official_lower.contains("productive")
            || official_lower.contains("fast"),
        "Official site should mention reliability/performance/productivity"
    );

    // --- Cross-source verification ---
    // Both sources should agree that Rust exists and is a programming language
    assert!(
        wiki_lower.contains("rust") && official_lower.contains("rust"),
        "Both sources should mention Rust"
    );

    // Find elements on the official site
    let (elements, success) =
        browser_action(&tool, json!({ "action": "find_elements", "selector": "a" })).await;
    assert!(success, "find_elements failed: {elements}");
    assert!(
        elements.contains("Found"),
        "Should report element count: {elements}"
    );

    // Clean up
    let (_, success) = browser_action(&tool, json!({ "action": "close" })).await;
    assert!(success, "close failed");
}
