use super::*;
use serde_json::json;

// -------------------------------------------------------------------
// Plugin protocol v2 stderr dispatch tests (W3.F2).
// -------------------------------------------------------------------

// ---- risk approval gate ----

// ---------------------------------------------------------------
// Phase 2-B SessionScope migration tests (PR #1198 follow-up).
//
// These pin the new scope-aware code path. They collapse the
// bespoke `resolve_plugin_input_path` / `absolutize_path_in_work_dir`
// / `resolve_slides_style_in_work_dir` validators behind a single
// `classify_lexical_path` gate so the 4-round #1186 traversal
// hardening + the #1189 workspace-root rescue have one home.
//
// The legacy fallback path (no scope) is independently exercised
// by the existing `rewrite_workspace_file_args_*` tests above,
// plus the `legacy_workspace_root_rescue_still_works_when_no_scope`
// pin further down which calls the resolver directly.
// ---------------------------------------------------------------

fn input_path_def(key: &str) -> PluginToolDef {
    PluginToolDef {
        name: format!("phase2b_{key}_tool"),
        description: "Phase 2-B fixture".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {key: {"type": "string"}}
        }),
        contexts: vec![],
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    }
}

fn solo_scope_at(root: &std::path::Path) -> SessionScope {
    SessionScope::solo(root.to_path_buf(), vec![]).expect("build solo scope")
}

// ---- mofa_slides style pre-flight validator ----
//
// These cover the synth-ack gap closed in
// `Tool::pre_flight_validate` for `mofa_slides`: invalid `style=`
// values used to slip past the spawn_only intercept and the LLM
// never saw the plugin's later `success:false`. The pre-flight now
// catches bare-name styles synchronously so the LLM gets a
// `[VALIDATION FAILED]` tool_result instead of the misleading
// synth-ack.

// -----------------------------------------------------------------
// Codex round-2 BLOCKER 1 (PR #1327 review): canonical-classify
// symlink-escape tests. A skill_dir containing
// `link -> /outside` previously let
// `<skill_dir>/link/secret` slip through as `InSkillDir`.
// These pin the fix.
// -----------------------------------------------------------------

#[cfg(unix)]
#[test]
fn plugin_refuses_input_path_using_symlink_escape_inside_skill_dir() {
    // Build:
    //   workspace/              (scope workspace)
    //   skill_dir/link -> outside/
    //   outside/secret.txt      (the file the symlink targets)
    //
    // Attempting to read `<skill_dir>/link/secret.txt` must be
    // refused — the canonical classify path resolves the symlink
    // before the prefix comparison so the candidate lands at
    // `outside/secret.txt`, which is outside the skill_dir.
    let workspace = tempfile::tempdir().expect("workspace dir");
    let skill_dir = tempfile::tempdir().expect("skill dir");
    let outside = tempfile::tempdir().expect("outside dir");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, b"AGENT_MUST_NEVER_READ_THIS").unwrap();
    let link = skill_dir.path().join("link");
    std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");

    // The skill_read_zone is the canonical (resolved) skill_dir,
    // mirroring what `runtime/session.rs` /  `chat.rs` /
    // `ActorFactory::spawn` configure after the round-2 BLOCKER 2
    // fix-closed canonicalisation.
    let canonical_skill_dir = std::fs::canonicalize(skill_dir.path()).expect("canonicalize");
    let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
        .expect("build solo scope")
        .with_skill_read_zones(vec![canonical_skill_dir])
        .expect("attach skill_read_zone");

    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("audio_path"),
        PathBuf::from("/bin/true"),
    );

    // Plugin would pass `<skill_dir>/link/secret.txt` lexically.
    let candidate = link.join("secret.txt").to_string_lossy().into_owned();
    let err = tool
        .rewrite_args_with_scope(
            &json!({"audio_path": candidate.clone()}),
            &scope,
            scope.workspace(),
        )
        .expect_err(
            "scope must refuse a skill_dir/symlink/<file> candidate after canonical classify",
        );
    let msg = err.to_string();
    assert!(
        msg.contains(&candidate),
        "error must echo the rejected raw path: {msg}",
    );
    // Wording follows the OutOfScope arm of `accept_for_intent` — the
    // canonical classify drops the candidate out of every zone, so
    // the scope-aware refusal text applies.
    assert!(
        msg.contains("escapes plugin work dir"),
        "error must surface OutOfScope refusal wording: {msg}",
    );
}

#[cfg(unix)]
#[test]
fn plugin_refuses_output_path_using_symlink_escape_inside_workspace() {
    // Same symlink-escape closure for output keys. Build:
    //   workspace/link -> outside/
    //   outside/                 (writable bait location)
    //
    // The bespoke #1186 / #1189 chain refused this case because of
    // its `..` guard; the SessionScope contract has to reach the
    // same answer without `..` — the canonical classify finds the
    // candidate is outside the workspace once the symlink is
    // resolved and refuses the write.
    let workspace = tempfile::tempdir().expect("workspace dir");
    let outside = tempfile::tempdir().expect("outside dir");
    let link = workspace.path().join("link");
    std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");

    let scope = solo_scope_at(workspace.path());
    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("out"),
        PathBuf::from("/bin/true"),
    );

    let candidate = link.join("artifact.bin").to_string_lossy().into_owned();
    let err = tool
        .rewrite_args_with_scope(
            &json!({"out": candidate.clone()}),
            &scope,
            scope.workspace(),
        )
        .expect_err(
            "scope must refuse a workspace/symlink/<artifact> output candidate \
                 after canonical classify",
        );
    let msg = err.to_string();
    assert!(
        msg.contains(&candidate),
        "error must echo the rejected raw path: {msg}",
    );
}

#[cfg(unix)]
const TEST_PLUGIN_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(unix)]
fn make_tool_def(name: &str, desc: &str) -> PluginToolDef {
    PluginToolDef {
        name: name.to_string(),
        description: desc.to_string(),
        input_schema: json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
        contexts: vec![],
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    }
}

#[cfg(unix)]
fn write_test_script(path: &std::path::Path, content: &str) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.sync_all().unwrap();
    drop(f);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // On Linux overlayfs (Docker), the kernel may still report ETXTBSY
    // briefly after closing. A short sleep allows the inode to settle.
    // macOS doesn't use overlayfs so this is skipped there.
    #[cfg(target_os = "linux")]
    std::thread::sleep(std::time::Duration::from_millis(50));
}

#[tokio::test]
#[cfg(unix)]
async fn continuation_owner_comes_only_from_host_tool_context() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("owner.sh");
    write_test_script(
        &script,
        "#!/bin/sh\nread INPUT || true\nprintf '%s' \"${OCTOS_CONTINUATION_OWNER:-missing}\"\n",
    );
    let mut def = make_tool_def("owner", "report execution identity");
    def.env.push("OCTOS_CONTINUATION_OWNER".into());
    let tool = PluginTool::new("p".into(), def, script)
        .with_extra_env(vec![("OCTOS_CONTINUATION_OWNER".into(), "forged".into())]);
    assert_eq!(tool.execute(&json!({})).await.unwrap().output, "missing");
    let ctx = crate::tools::ToolContext {
        continuation_owner: Some("session:profile:local:tui#peer-maker".into()),
        ..crate::tools::ToolContext::zero()
    };
    let r = crate::tools::TOOL_CTX
        .scope(ctx, tool.execute(&json!({})))
        .await
        .unwrap();
    assert_eq!(r.output, "session:profile:local:tui#peer-maker");
}

#[test]
fn plugin_model_media_snapshots_deduplicates_and_rejects_unsafe_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bytes = b"\x89PNG\r\n\x1a\n";
    std::fs::write(root.join("a.png"), bytes).unwrap();
    std::fs::write(root.join("b.png"), bytes).unwrap();
    let (media, aliases) =
        plugin_model_media(Some(&json!(["a.png", "b.png"])), Some(root)).unwrap();
    assert_eq!(media.len(), 1);
    assert_eq!(aliases.len(), 2);
    assert_eq!(aliases[0]["image_id"], aliases[1]["image_id"]);
    std::fs::write(root.join("a.png"), b"changed").unwrap();
    assert_eq!(std::fs::read(&media[0]).unwrap(), bytes);
    assert!(plugin_model_media(Some(&json!(["a.png"])), Some(root)).is_err());
    for bad in [
        json!(["../escape.png"]),
        json!(["/etc/passwd"]),
        json!([null]),
        json!("a.png"),
    ] {
        assert!(plugin_model_media(Some(&bad), Some(root)).is_err());
    }
    assert!(plugin_model_media(Some(&json!(["b.png"])), None).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("b.png"), root.join("link.png")).unwrap();
        assert!(plugin_model_media(Some(&json!(["link.png"])), Some(root)).is_err());
        std::os::unix::fs::symlink(root, root.join("parent-link")).unwrap();
        assert!(plugin_model_media(Some(&json!(["parent-link/b.png"])), Some(root)).is_err());
    }
}

#[tokio::test]
#[cfg(unix)]
async fn plugin_model_media_survives_real_process_envelope() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("screen.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let script = dir.path().join("plugin.sh");
    write_test_script(
        &script,
        "#!/bin/sh\nprintf '%s\\n' '{\"success\":true,\"output\":\"Native crop + geometry\",\"model_media\":[\"screen.png\"]}'\n",
    );
    let tool = PluginTool::new(
        "fixture".into(),
        make_tool_def("image_packet", "packet"),
        script,
    )
    .with_work_dir(dir.path().to_path_buf())
    .with_timeout(TEST_PLUGIN_TIMEOUT);
    let r = tool.execute(&json!({})).await.unwrap();
    assert!(r.success, "{}", r.output);
    assert_eq!(r.model_media.len(), 1);
    assert!(
        r.files_to_send.is_empty(),
        "model evidence is not a chat delivery request"
    );
    assert_eq!(
        std::fs::read(&r.model_media[0]).unwrap(),
        b"\x89PNG\r\n\x1a\n"
    );
}
