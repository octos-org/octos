//! Gap 4.1 — the `run_pipeline` tool must advertise a `pipeline` enum that
//! reflects the LIVE discovery list, not a hard-coded `["deep_research"]`.
//!
//! Live mini5 soak failure: `run_pipeline deep_research` returned
//! `Available: (none)` because the `mofa-research` skill carrying
//! `deep_research.dot` had drifted off the profile. Bundling the `.dot`
//! (octos-agent) plus making the advertised enum match reality (here) keeps
//! the model from emitting names that don't exist — and surfaces the names
//! that DO exist.

use std::path::PathBuf;
use std::sync::Arc;

use octos_agent::Tool;
use octos_pipeline::RunPipelineTool;

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

async fn make_tool_with_data(working: &std::path::Path, data: &std::path::Path) -> RunPipelineTool {
    let memory_dir = data.join("episodes");
    let memory = Arc::new(octos_memory::EpisodeStore::open(&memory_dir).await.unwrap());
    RunPipelineTool::new(
        Arc::new(MockProvider) as Arc<dyn octos_llm::LlmProvider>,
        memory,
        PathBuf::from(working),
        PathBuf::from(data),
    )
}

fn enum_values(schema: &serde_json::Value) -> Vec<String> {
    schema["properties"]["pipeline"]["enum"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// RED today: the enum is hard-coded to `["deep_research"]`, so a discovered
/// pipeline with a DIFFERENT name never shows up.
#[tokio::test]
async fn pipeline_enum_reflects_discovered_pipelines() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // Install a pipeline with a name that is NOT the legacy hard-coded one.
    let pipelines_dir = data.path().join("pipelines");
    std::fs::create_dir_all(&pipelines_dir).unwrap();
    std::fs::write(
        pipelines_dir.join("custom_flow.dot"),
        "digraph custom_flow { a [prompt=\"hi\"] }",
    )
    .unwrap();

    let tool = make_tool_with_data(working.path(), data.path()).await;
    let schema = tool.input_schema();
    let values = enum_values(&schema);

    assert!(
        values.iter().any(|v| v == "custom_flow"),
        "advertised pipeline enum must include the discovered 'custom_flow' \
         pipeline, got {values:?}"
    );
}

/// When no pipelines can be discovered, the schema must NOT crash and must
/// fall back sensibly (keep the `deep_research` baseline name advertised so
/// the model still has the sanctioned generic pipeline available, matching
/// the bundled fallback).
#[tokio::test]
async fn pipeline_enum_falls_back_when_no_discovery() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // Intentionally no pipelines dir / no .dot files anywhere.

    let tool = make_tool_with_data(working.path(), data.path()).await;
    let schema = tool.input_schema();
    let values = enum_values(&schema);

    assert!(
        !values.is_empty(),
        "empty discovery must still advertise a non-empty fallback enum"
    );
    assert!(
        values.iter().any(|v| v == "deep_research"),
        "no-discovery fallback must advertise the baseline 'deep_research' name, got {values:?}"
    );
}

/// Gap 4.1 NIT 2 — the fallback enum must NOT advertise a pipeline the tool
/// cannot actually resolve. RED before the fix: with NO discovery (no
/// bootstrap, empty dirs) the enum advertised `deep_research` (via the
/// unconditional fallback), but `pre_flight_validate("deep_research")`
/// FAILED with `Available: (none)` — a masking lie. After: the named
/// resolver falls back to the embedded bundled bytes for the sanctioned
/// `deep_research`, so advertise == resolvable on every path, including a
/// degraded filesystem where bootstrap's disk write failed.
#[tokio::test]
async fn fallback_advertised_deep_research_actually_resolves() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    // Intentionally no pipelines dir, no bootstrap, no .dot files anywhere.

    let tool = make_tool_with_data(working.path(), data.path()).await;

    // (a) advertised
    let values = enum_values(&tool.input_schema());
    assert!(
        values.iter().any(|v| v == "deep_research"),
        "fallback must advertise deep_research, got {values:?}"
    );

    // (b) MUST actually resolve (this is the masking guard).
    let args = serde_json::json!({ "pipeline": "deep_research", "input": "x" });
    tool.pre_flight_validate(&args).await.expect(
        "an advertised pipeline MUST be resolvable — the enum fallback must not \
         advertise a name the tool cannot resolve (NIT 2 masking guard)",
    );
}

/// `with_octos_home` must add `<octos_home>/pipelines` as a discovery search
/// path so an operator-installed user pipeline written there is advertised in
/// the enum. (NB: the BUNDLED generic pipelines now land in the dedicated
/// `<octos_home>/bundled-pipelines` dir via `bootstrap_bundled_pipelines`, NOT
/// here — see the bundled-fallback tests below.)
#[tokio::test]
async fn pipeline_enum_includes_octos_home_pipelines() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    // Operator-installed user pipeline lands in <octos_home>/pipelines.
    // Use a NON-baseline name so a hard-coded fallback can't satisfy this.
    let home_pipelines = octos_home.path().join("pipelines");
    std::fs::create_dir_all(&home_pipelines).unwrap();
    std::fs::write(
        home_pipelines.join("home_bundled_flow.dot"),
        "digraph home_bundled_flow { a [prompt=\"hi\"] }",
    )
    .unwrap();

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));
    let schema = tool.input_schema();
    let values = enum_values(&schema);

    assert!(
        values.iter().any(|v| v == "home_bundled_flow"),
        "with_octos_home must surface <octos_home>/pipelines/home_bundled_flow.dot \
         in the advertised enum, got {values:?}"
    );
}

/// Collect every tool NAME the host actually registers for a pipeline
/// worker: the built-in tool registry (`ToolRegistry::with_builtins`)
/// PLUS every tool exported by the bundled app-skill / platform-skill
/// manifests (these are loaded into the worker registry via
/// `plugin_dirs`, see `handler.rs::CodergenHandler::execute`). A bundled
/// `.dot` may only reference / allow-list names in this set — otherwise
/// the worker's allow-list policy (handler.rs) silently drops the tool
/// and the node cannot do its job at runtime.
fn registered_tool_names() -> std::collections::HashSet<String> {
    let mut names: std::collections::HashSet<String> =
        octos_agent::ToolRegistry::with_builtins(std::env::temp_dir())
            .tool_names()
            .into_iter()
            .collect();

    // Bundled plugin (app-skill + platform-skill) tools the pipeline
    // worker loads via plugin_dirs. Parse each manifest's `tools[].name`.
    let manifests = octos_agent::bundled_app_skills::BUNDLED_APP_SKILLS
        .iter()
        .chain(octos_agent::bundled_app_skills::PLATFORM_SKILLS.iter())
        .map(|&(_, _, _, manifest_json)| manifest_json);
    for manifest_json in manifests {
        let manifest: serde_json::Value =
            serde_json::from_str(manifest_json).expect("bundled manifest must be valid JSON");
        if let Some(tools) = manifest.get("tools").and_then(|t| t.as_array()) {
            for tool in tools {
                if let Some(name) = tool.get("name").and_then(|n| n.as_str()) {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// Collect every tool reference a bundled `.dot` carries: the union of
/// every node's `tools=` allow-list across all nodes. This is the set
/// the handler turns into a `ToolPolicy.allow` list — anything not
/// registered is unreachable at runtime.
fn dot_tool_references(dot: &str) -> std::collections::BTreeSet<String> {
    let graph = octos_pipeline::parser::parse_dot(dot).expect("bundled .dot must parse");
    let mut refs = std::collections::BTreeSet::new();
    for node in graph.nodes.values() {
        for tool in &node.tools {
            let t = tool.trim();
            if !t.is_empty() {
                refs.insert(t.to_string());
            }
        }
    }
    refs
}

/// Gap 4.1 BLOCKER 1 — the missing test class. Every tool a bundled
/// `.dot` references (its `tools=` allow-list) MUST resolve to a tool
/// the host actually registers. RED on e31665ca: `deep_research.dot`
/// allow-listed `deep_search`, but octos registers that tool as `search`
/// (the in-process `DeepSearchTool` names itself `search`; the
/// deep-search app-skill manifest exports `search`). The pipeline worker
/// applies the DOT allow-list (handler.rs), so `deep_search` was unknown
/// → the node could never run the web search it was built for.
#[test]
fn every_bundled_dot_tool_reference_is_registered() {
    let registered = registered_tool_names();
    assert!(
        registered.contains("search"),
        "precondition: the registered set must include the `search` tool \
         (DeepSearchTool / deep-search app-skill), got {} names",
        registered.len()
    );

    for &(file_name, dot) in octos_agent::bundled_pipelines::BUNDLED_PIPELINES {
        let refs = dot_tool_references(dot);
        let unregistered: Vec<&String> = refs.iter().filter(|r| !registered.contains(*r)).collect();
        assert!(
            unregistered.is_empty(),
            "bundled pipeline '{file_name}' references tool(s) that octos does NOT \
             register: {unregistered:?} (every `tools=` entry must be a name the \
             host registers — builtins or a bundled-skill manifest export). \
             All references: {refs:?}"
        );
    }
}

/// Gap 4.1 BLOCKER 2 (chat/serve path) — those hosts bootstrap the bundle
/// into `<data_dir>/bundled-pipelines` and register it via
/// `with_bundled_pipelines_root(data_dir)` (NOT `with_octos_home`). The
/// invariant: the dir bootstrap writes into is exactly the dir discovery
/// searches. RED before the fix: chat wrote the bundle to
/// `<data_dir>/pipelines` (which also shadowed installs) and never
/// registered a dedicated bundled dir.
#[tokio::test]
async fn chat_path_bootstrap_dir_equals_search_dir() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();

    // chat.rs bootstraps the bundle into `data_dir`.
    let written = octos_agent::bootstrap::bootstrap_bundled_pipelines(data.path());
    assert!(written >= 1, "bootstrap must write at least deep_research");

    // chat.rs builds the tool with `with_bundled_pipelines_root(data_dir)`.
    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_bundled_pipelines_root(PathBuf::from(data.path()));

    let values = enum_values(&tool.input_schema());
    assert!(
        values.iter().any(|v| v == "deep_research"),
        "chat path: bootstrapped deep_research must be discoverable, got {values:?}"
    );
    let args = serde_json::json!({ "pipeline": "deep_research", "input": "x" });
    tool.pre_flight_validate(&args)
        .await
        .expect("chat path: bootstrapped deep_research must resolve + validate");
}

/// Gap 4.1 BLOCKER 2 + 3 (gateway path) — the gateway bootstraps into
/// `<effective_octos_home>/bundled-pipelines` and registers discovery via
/// `with_octos_home(effective_octos_home)`. An installed skill copy in
/// `<octos_home>/skills/<x>/deep_research.dot` must WIN over the bundled
/// one, AND the bundled one must still resolve when no install exists.
#[tokio::test]
async fn gateway_path_installed_wins_and_bundled_discovers() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    octos_agent::bootstrap::bootstrap_bundled_pipelines(octos_home.path());

    // No install yet: bundled must resolve.
    {
        let tool = make_tool_with_data(working.path(), data.path())
            .await
            .with_octos_home(PathBuf::from(octos_home.path()));
        let dot = tool
            .resolve_named_for_test("deep_research")
            .await
            .expect("bundled deep_research must resolve via with_octos_home");
        assert!(
            dot.contains("digraph deep_research"),
            "bundled copy must resolve when no install exists"
        );
        // The fixed tool name must be present (regression guard for Blocker 1),
        // now including the `write_file` grant so workers can write their
        // `findings-{label}.md` deliverable.
        assert!(
            dot.contains("tools=\"search,read_file,write_file\""),
            "bundled deep_research must allow-list the registered `search` tool (+ write_file for the findings deliverable), not `deep_search`; got: {dot}"
        );
    }

    // Install a skill copy of the same name — it must now win.
    let skill_dir = octos_home.path().join("skills").join("mofa-research");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("deep_research.dot"),
        "digraph deep_research { installed [prompt=\"INSTALLED\"] }",
    )
    .unwrap();

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));
    let dot = tool
        .resolve_named_for_test("deep_research")
        .await
        .expect("installed deep_research must resolve");
    assert!(
        dot.contains("INSTALLED"),
        "installed skill deep_research.dot must win over the bundled copy, got: {dot}"
    );
}

/// Gap 4.1 BLOCKER 2 (`.dot`-suffixed input bypasses installed-wins) —
/// `resolve("deep_research.dot")` and `resolve("deep_research")` must behave
/// IDENTICALLY through the full tool resolve path (discovery + embedded
/// fallback):
///   - When an installed `skills/<x>/deep_research.dot` exists, BOTH forms
///     resolve the INSTALLED copy (installed-wins). RED on 344d0df1: the
///     `.dot` form missed discovery's stem comparison → discovery Err → the
///     embedded-bytes fallback's `want == file_name` matched `deep_research.dot`
///     → BUNDLED won over INSTALLED.
///   - When nothing is installed, BOTH forms fall to the embedded bundled
///     bytes (fallback is the whole point of bundling).
#[tokio::test]
async fn bare_name_obeys_installed_wins_and_dot_path_is_rejected() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    // Nothing installed: BOTH input forms fall to the embedded bundled bytes.
    {
        let tool = make_tool_with_data(working.path(), data.path())
            .await
            .with_octos_home(PathBuf::from(octos_home.path()));
        for input in ["deep_research"] {
            let dot = tool
                .resolve_named_for_test(input)
                .await
                .unwrap_or_else(|e| {
                    panic!("`{input}` must fall back to embedded bundled bytes, got err: {e}")
                });
            assert!(
                dot.contains("digraph deep_research"),
                "`{input}`: embedded bundled deep_research must resolve when nothing installed, got: {dot}"
            );
        }
    }

    // Install a skill copy of the same name — BOTH input forms must now win
    // it (installed-wins), never the embedded bundled bytes.
    let skill_dir = octos_home.path().join("skills").join("mofa-research");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("deep_research.dot"),
        "digraph deep_research { installed [prompt=\"INSTALLED\"] }",
    )
    .unwrap();

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));
    for input in ["deep_research"] {
        let dot = tool
            .resolve_named_for_test(input)
            .await
            .unwrap_or_else(|e| panic!("`{input}` must resolve the installed copy, got err: {e}"));
        assert!(
            dot.contains("INSTALLED"),
            "`{input}`: installed deep_research.dot must win over embedded bundled bytes, got: {dot}"
        );
    }

    // Security: the `.dot`-suffixed / path form is REJECTED for agent runs — a
    // model could otherwise point `run_pipeline` at an on-disk `.dot` it wrote
    // (discovery's direct-path read). Agents use the bare sanctioned name.
    let err = tool
        .resolve_named_for_test("deep_research.dot")
        .await
        .expect_err("`.dot`-suffixed / path input must be rejected for agent runs");
    assert!(
        err.to_string().contains("file paths are not accepted"),
        "rejection must name the path restriction, got: {err}"
    );
}

/// Gap 4.1 BLOCKER 1 (standalone gateway child-profile uses the wrong
/// pipeline root) — encodes the standalone `octos gateway` invariant at the
/// resolution boundary:
///
/// On the standalone path (no `--octos-home`), the gateway bootstraps bundled
/// pipelines into `effective_octos_home` (= `data_dir`), but the child-profile
/// factory historically rooted `run_pipeline` at `project_dir` (= `cwd/.octos`)
/// — a DIFFERENT dir bootstrap never wrote. This test proves:
///   1. A tool rooted at `effective_octos_home` (the bootstrap dir) discovers
///      an installed GLOBAL pipeline there and lets it WIN over the bundled
///      fallback (bootstrap-dir == search-dir → installed-wins).
///   2. A tool rooted at the WRONG `project_dir` (where bootstrap never wrote)
///      cannot see that installed pipeline at all — the exact 344d0df1 defect
///      that let the embedded fallback beat an installed global pipeline.
#[tokio::test]
async fn standalone_gateway_child_profile_roots_pipeline_at_bootstrap_dir() {
    // Standalone layout: cwd/.octos (project_dir) and data_dir
    // (effective_octos_home) are DISTINCT dirs.
    let cwd = tempfile::tempdir().unwrap();
    let project_dir = cwd.path().join(".octos");
    std::fs::create_dir_all(&project_dir).unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let effective_octos_home = data_dir.path();
    // Separate per-session data dirs so the two RunPipelineTool instances
    // below each open their own EpisodeStore (redb is single-writer).
    let session_data_correct = tempfile::tempdir().unwrap();
    let session_data_wrong = tempfile::tempdir().unwrap();

    // Gateway bootstraps the bundled pipelines into effective_octos_home.
    octos_agent::bootstrap::bootstrap_bundled_pipelines(effective_octos_home);

    // Operator installs a GLOBAL deep_research under effective_octos_home/skills.
    let global_skill = effective_octos_home.join("skills").join("mofa-research");
    std::fs::create_dir_all(&global_skill).unwrap();
    std::fs::write(
        global_skill.join("deep_research.dot"),
        "digraph deep_research { installed [prompt=\"GLOBAL_INSTALLED\"] }",
    )
    .unwrap();

    // (1) CORRECT root = effective_octos_home (bootstrap-dir == search-dir):
    // the installed GLOBAL copy must win over the bundled fallback.
    let correct = make_tool_with_data(cwd.path(), session_data_correct.path())
        .await
        .with_octos_home(PathBuf::from(effective_octos_home));
    let dot = correct
        .resolve_named_for_test("deep_research")
        .await
        .expect("installed global deep_research must resolve at the bootstrap dir");
    assert!(
        dot.contains("GLOBAL_INSTALLED"),
        "child-profile pipeline rooted at effective_octos_home must let the installed \
         global pipeline win over the bundled fallback, got: {dot}"
    );

    // (2) WRONG root = project_dir (cwd/.octos), where bootstrap NEVER wrote and
    // no install exists: the installed global pipeline is invisible. This is
    // the 344d0df1 defect — discovery there falls through to the embedded
    // bundled bytes instead of the installed global copy.
    let wrong = make_tool_with_data(cwd.path(), session_data_wrong.path())
        .await
        .with_octos_home(project_dir.clone());
    let dot_wrong = wrong
        .resolve_named_for_test("deep_research")
        .await
        .expect("embedded bundled fallback still resolves");
    assert!(
        !dot_wrong.contains("GLOBAL_INSTALLED"),
        "rooting at project_dir must NOT see the global install under \
         effective_octos_home — demonstrating why the wrong root breaks \
         installed-wins (it fell to the embedded bundled copy instead)"
    );
}

/// Gap 4.1 (codex review) — the embedded bundled fallback must fire ONLY on a
/// TRUE discovery miss, never to MASK an installed-but-unreadable pipeline.
///
/// `PipelineDiscovery::resolve` errors in two distinct situations:
///   - TRUE MISS: no candidate file located anywhere → fallback to the bundled
///     bytes is correct (covered by the other tests here).
///   - FOUND-BUT-UNREADABLE: discovery LOCATED an installed `deep_research.dot`
///     but failed to read/parse it (read/permission/UTF-8 error) → falling back
///     to the bundled copy would MASK the broken install and let the fallback
///     out-rank a present installed pipeline. That violates "fallback only on a
///     true miss / can never out-rank an installed pipeline."
///
/// Here the installed `skills/<x>/deep_research.dot` is created as a DIRECTORY
/// (not a regular file): discovery still locates it (the `.dot` extension scan
/// matches a dir entry, stem `deep_research`), but `read_to_string` on a
/// directory fails — the canonical found-but-unreadable case, cross-platform.
///
/// RED on 134623eb: `resolve_named_with_bundled_fallback` fell back on ANY
/// discovery `Err`, so the read failure was silently masked by the bundled
/// bytes (`Ok(...)` containing `digraph deep_research`). GREEN after the fix:
/// the read error is propagated (NOT masked) because a candidate WAS located.
#[tokio::test]
async fn corrupt_installed_pipeline_is_not_masked_by_bundled_fallback() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    // Bootstrap the bundled fallback so the embedded bytes ARE available — the
    // whole point is that the fallback exists yet must NOT mask the broken
    // install.
    octos_agent::bootstrap::bootstrap_bundled_pipelines(octos_home.path());

    // Install a copy of the SAME pipeline name that discovery can LOCATE but
    // not READ: a directory named `deep_research.dot` (extension scan matches,
    // `read_to_string` on a dir errors).
    let skill_dir = octos_home.path().join("skills").join("mofa-research");
    std::fs::create_dir_all(skill_dir.join("deep_research.dot")).unwrap();

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));

    let result = tool.resolve_named_for_test("deep_research").await;
    assert!(
        result.is_err(),
        "an installed-but-unreadable deep_research.dot must surface a read error, \
         NOT be silently masked by the embedded bundled bytes; got Ok(..): {:?}",
        result.as_ref().ok()
    );
    let dot = result.unwrap_or_default();
    assert!(
        !dot.contains("digraph deep_research"),
        "the embedded bundled copy must NOT out-rank a located-but-unreadable \
         installed pipeline, got bundled bytes: {dot}"
    );
}

/// Gap 4.1 (codex review) — the mirror of the discovery-layer
/// `bare_name_with_coincidental_non_dot_path_is_a_true_miss_not_read` test, but
/// asserted at the TOOL boundary: a coincidental non-`.dot` entry that merely
/// shares the bare pipeline name must NOT block the embedded bundled fallback.
///
/// Setup: a DIRECTORY `<octos_home>/pipelines/deep_research` (coincidental,
/// non-`.dot`) exists in a search path, but there is NO `deep_research.dot`
/// installed anywhere. The embedded bundled bytes ARE available.
///
/// RED on ffdfdb98: step 2 of `PipelineDiscovery::resolve` treated the
/// coincidental directory as the located pipeline, `read_to_string` failed, and
/// `resolve` returned `Read` — which `resolve_named_with_bundled_fallback` does
/// NOT fall back on (it only falls back on a TRUE `NotFound` miss). So the
/// bundled `deep_research` was unreachable and `resolve_named_for_test` errored.
/// GREEN after: step 2 ignores the non-`.dot` directory, resolution falls
/// through to `NotFound`, and the embedded bundled `deep_research` fires.
#[tokio::test]
async fn coincidental_non_dot_path_does_not_block_bundled_fallback() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    // A coincidental DIRECTORY named exactly like the bare pipeline name in a
    // search path. There is NO `deep_research.dot` anywhere on disk.
    let home_pipelines = octos_home.path().join("pipelines");
    std::fs::create_dir_all(home_pipelines.join("deep_research")).unwrap();

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));

    // The coincidental non-`.dot` path must NOT mis-classify as Read and block
    // the embedded bundled fallback — `deep_research` must resolve to the
    // bundled bytes.
    let dot = tool.resolve_named_for_test("deep_research").await.expect(
        "a coincidental non-`.dot` directory sharing the bare name must NOT block \
             the embedded bundled fallback (it is a true miss, not a Read failure)",
    );
    assert!(
        dot.contains("digraph deep_research"),
        "the embedded bundled deep_research must resolve despite the coincidental \
         non-`.dot` directory, got: {dot}"
    );

    // And the full tool path (resolve + parse + validate) must accept it.
    let args = serde_json::json!({ "pipeline": "deep_research", "input": "x" });
    tool.pre_flight_validate(&args).await.expect(
        "bundled deep_research must pass pre_flight_validate even when a coincidental \
         non-`.dot` directory shadows the bare name",
    );
}

/// Cross-crate guard: every pipeline bundled by `octos_agent` must parse and
/// validate clean against THIS crate's parser/validator — otherwise
/// `pre_flight_validate` would reject the bundled fallback the moment the
/// model named it.
#[test]
fn bundled_pipelines_parse_and_validate_clean() {
    for &(file_name, dot) in octos_agent::bundled_pipelines::BUNDLED_PIPELINES {
        let graph = octos_pipeline::parser::parse_dot(dot)
            .unwrap_or_else(|e| panic!("bundled pipeline '{file_name}' fails to parse: {e}"));
        // Main's validate-before-execute (#1374) split the API: `validate()`
        // now returns a pass/fail `Result`, while `diagnostics()` returns the
        // full diagnostic list this guard inspects for error-severity entries.
        let diags = octos_pipeline::validate::diagnostics(&graph);
        assert!(
            !octos_pipeline::validate::has_errors(&diags),
            "bundled pipeline '{file_name}' has validation errors: {:?}",
            diags
                .iter()
                .filter(|d| d.severity == octos_pipeline::validate::Severity::Error)
                .collect::<Vec<_>>()
        );
    }
}

/// End-to-end: after `bootstrap_bundled_pipelines` writes into
/// `<octos_home>/bundled-pipelines`, a `RunPipelineTool` built with
/// `with_octos_home` advertises `deep_research` AND can `resolve` it. This is
/// the exact path the mini5 soak missed (skill drift → `Available: (none)`).
#[tokio::test]
async fn bootstrap_then_discover_deep_research_end_to_end() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let octos_home = tempfile::tempdir().unwrap();

    let written = octos_agent::bootstrap::bootstrap_bundled_pipelines(octos_home.path());
    assert!(written >= 1, "bootstrap must write at least deep_research");

    let tool = make_tool_with_data(working.path(), data.path())
        .await
        .with_octos_home(PathBuf::from(octos_home.path()));

    let values = enum_values(&tool.input_schema());
    assert!(
        values.iter().any(|v| v == "deep_research"),
        "bootstrapped deep_research must be advertised, got {values:?}"
    );

    // And it must actually resolve (pre_flight_validate's resolve step).
    let args = serde_json::json!({ "pipeline": "deep_research", "input": "x" });
    tool.pre_flight_validate(&args)
        .await
        .expect("bootstrapped deep_research must pass pre_flight_validate");
}

/// Routing guidance ("Jingkang artifact" routing-collapse fix). The tool
/// exposes exactly one pipeline (`deep_research`), and the agent force-fit a
/// CODE-REVIEW task onto it — the web-research-synthesis pipeline then ran
/// over a shared workspace and recalled a stale unrelated research artifact.
///
/// We cannot reliably classify intent in code without a brittle classifier,
/// so the enforcement is a STRONG description/schema guardrail: both the
/// tool `description()` and the `pipeline` arg's enum-doc must state in plain
/// terms that `deep_research` is for multi-source WEB-research synthesis ONLY
/// and MUST NOT be used for code review / local-codebase analysis / anything
/// answerable from the working directory (those use the direct
/// file/shell/grep/read tools). This test pins that guidance so it cannot
/// silently regress.
#[tokio::test]
async fn run_pipeline_guidance_steers_code_review_away_from_deep_research() {
    let working = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let tool = make_tool_with_data(working.path(), data.path()).await;

    let description = tool.description().to_ascii_lowercase();
    let schema = tool.input_schema();
    let pipeline_desc = schema["properties"]["pipeline"]["description"]
        .as_str()
        .expect("pipeline arg must carry a description")
        .to_ascii_lowercase();

    // The two surfaces the model actually reads when deciding to call.
    let combined = format!("{description}\n{pipeline_desc}");

    // 1. It must scope deep_research to multi-source WEB research synthesis.
    assert!(
        combined.contains("web") && combined.contains("research"),
        "guidance must scope deep_research to web-research synthesis; got:\n{combined}"
    );

    // 2. It must explicitly steer code review away.
    assert!(
        combined.contains("code review") || combined.contains("code-review"),
        "guidance must explicitly mention code review as a NON-use; got:\n{combined}"
    );

    // 3. It must explicitly steer local-codebase / working-directory tasks away.
    assert!(
        combined.contains("local") || combined.contains("codebase"),
        "guidance must mention local-codebase analysis as a NON-use; got:\n{combined}"
    );
    assert!(
        combined.contains("working directory") || combined.contains("working-directory"),
        "guidance must mention working-directory-answerable tasks as a NON-use; got:\n{combined}"
    );

    // 4. It must point such tasks at the direct file/shell tools instead.
    let names_direct_tools = ["read_file", "read", "grep", "shell", "list_dir"]
        .iter()
        .any(|t| combined.contains(t));
    assert!(
        names_direct_tools,
        "guidance must redirect code-review / local tasks to the direct \
         file/shell/grep/read tools; got:\n{combined}"
    );

    // 5. A clear MUST NOT / do-not prohibition (not merely a soft hint).
    assert!(
        combined.contains("must not") || combined.contains("do not use"),
        "guidance must carry a strong prohibition (MUST NOT / do not use), not \
         a soft suggestion; got:\n{combined}"
    );
}
