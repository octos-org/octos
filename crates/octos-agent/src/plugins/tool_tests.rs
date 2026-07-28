use super::*;
use crate::SilentReporter;
use serde_json::json;
use std::sync::Arc;

fn make_tool_def(name: &str, desc: &str) -> PluginToolDef {
    PluginToolDef {
        name: name.to_string(),
        description: desc.to_string(),
        input_schema: json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    }
}

#[test]
fn new_sets_defaults() {
    let def = make_tool_def("greet", "Say hello");
    let tool = PluginTool::new("my-plugin".into(), def, PathBuf::from("/bin/echo"));

    assert_eq!(tool.plugin_name, "my-plugin");
    assert_eq!(tool.timeout, PluginTool::DEFAULT_TIMEOUT);
    assert_eq!(tool.timeout, Duration::from_secs(600));
    assert!(tool.blocked_env.is_empty());
}

#[test]
fn with_blocked_env_sets_list() {
    let def = make_tool_def("t", "d");
    let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"))
        .with_blocked_env(vec!["SECRET".into(), "TOKEN".into()]);

    assert_eq!(tool.blocked_env, vec!["SECRET", "TOKEN"]);
}

#[test]
fn with_extra_env_sets_vars() {
    let def = make_tool_def("t", "d");
    let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo")).with_extra_env(vec![
        (
            "GEMINI_BASE_URL".into(),
            "https://api.r9s.ai/gemini/v1beta".into(),
        ),
        ("GEMINI_API_KEY".into(), "test-key".into()),
    ]);

    assert_eq!(tool.extra_env.len(), 2);
    assert_eq!(tool.extra_env[0].0, "GEMINI_BASE_URL");
    assert_eq!(tool.extra_env[1].0, "GEMINI_API_KEY");
}

#[test]
fn with_timeout_sets_custom() {
    let def = make_tool_def("t", "d");
    let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"))
        .with_timeout(Duration::from_secs(120));

    assert_eq!(tool.timeout, Duration::from_secs(120));
}

#[test]
fn trait_methods_delegate_to_tool_def() {
    let def = make_tool_def("my_tool", "A fine tool");
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

    assert_eq!(tool.name(), "my_tool");
    assert_eq!(tool.description(), "A fine tool");
    let schema = tool.input_schema();
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["msg"].is_object());
}

#[test]
fn rewrite_workspace_file_args_updates_audio_and_file_paths() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("mark.wav");
    let pdf = dir.path().join("deck.pdf");
    std::fs::write(&wav, b"wav").unwrap();
    std::fs::write(&pdf, b"pdf").unwrap();

    let def = PluginToolDef {
        name: "voice_tool".to_string(),
        description: "Voice tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "audio_path": {"type": "string"},
                "file_path": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(dir.path().to_path_buf());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "audio_path": "/home/user/uploads/mark.wav",
            "file_path": "deck.pdf",
        }))
        .unwrap();

    // `audio_path` (a fictional absolute path) cannot resolve
    // through the unified table — it's outside every allowed root
    // — and falls back to the legacy `resolve_path_in_work_dir`
    // filename match. `file_path` (`deck.pdf`) is workspace-relative
    // and resolves through the unified resolver, which returns the
    // lexical workspace path on purpose (the tool's `O_NOFOLLOW`
    // open is the symlink-safety gate; canonicalising here would
    // bypass it).
    assert_eq!(rewritten["audio_path"], wav.to_string_lossy().to_string());
    assert_eq!(rewritten["file_path"], pdf.to_string_lossy().to_string());
}

#[test]
fn rewrite_workspace_file_args_preserves_nested_workspace_paths() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("slides").join("demo");
    std::fs::create_dir_all(&nested).unwrap();
    let script = nested.join("script.js");
    std::fs::write(&script, b"export default [];").unwrap();

    let def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "Slides tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "input": {"type": "string"},
                "out": {"type": "string"},
                "slide_dir": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(dir.path().to_path_buf());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "input": "slides/demo/script.js",
            "out": "slides/demo/output/deck.pptx",
            "slide_dir": "slides/demo/output/imgs"
        }))
        .unwrap();

    // All three keys end up as lexical workspace paths: `input`
    // resolves through the unified resolver (workspace scope keeps
    // the lexical form so the leaf `O_NOFOLLOW` gate can refuse
    // symlinks), and `out` / `slide_dir` go through the
    // absolutize-only branch which has always been lexical.
    assert_eq!(rewritten["input"], script.to_string_lossy().to_string());
    assert_eq!(
        rewritten["out"],
        dir.path()
            .join("slides/demo/output/deck.pptx")
            .to_string_lossy()
            .to_string()
    );
    assert_eq!(
        rewritten["slide_dir"],
        dir.path()
            .join("slides/demo/output/imgs")
            .to_string_lossy()
            .to_string()
    );
}

#[test]
fn rewrite_workspace_file_args_recovers_basename_when_workspace_relative_missing() {
    // Codex review P2 (2026-05-13): when the LLM hallucinates a
    // directory prefix in front of a basename that exists at the
    // workspace root, the plugin filename fallback must rescue it.
    // The unified resolver succeeds for any syntactically valid
    // workspace-relative path even when the file is missing, so
    // the plugin code must require existence on the workspace scope
    // before accepting the resolver's result.
    let dir = tempfile::tempdir().unwrap();
    let mark = dir.path().join("mark.wav");
    std::fs::write(&mark, b"wav").unwrap();
    // Note: `uploads/mark.wav` deliberately does NOT exist.

    let def = PluginToolDef {
        name: "voice_tool".to_string(),
        description: "Voice tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"audio_path": {"type": "string"}}
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(dir.path().to_path_buf());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "audio_path": "uploads/mark.wav",
        }))
        .unwrap();

    // Must recover `<work_dir>/mark.wav` via the legacy filename
    // fallback, NOT return the missing `<work_dir>/uploads/mark.wav`.
    assert_eq!(rewritten["audio_path"], mark.to_string_lossy().to_string());
}

#[test]
fn rewrite_workspace_file_args_strips_redundant_skill_output_prefix_for_script_path() {
    // B1 fleet UX soak (mini2/iter1 + mini5/iter2): the modern
    // `runtime/session.rs` path chroots plugin `work_dir` into
    // `<workspace>/skill-output/`, while `write_file`'s base_dir
    // is the workspace ROOT. When the LLM passes the same
    // `skill-output/mofa-podcast/<file>.md` path to both, the
    // naive `work_dir.join(...)` doubles the prefix and the
    // plugin's `read_to_string` fails with `No such file or
    // directory (os error 2)`. The rewrite must detect this and
    // resolve the path against `work_dir` WITHOUT the redundant
    // prefix.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    let podcast_dir = skill_output.join("mofa-podcast");
    std::fs::create_dir_all(&podcast_dir).unwrap();
    let script = podcast_dir.join("octos_intro_script.md");
    std::fs::write(&script, b"# Podcast script").unwrap();

    let def = PluginToolDef {
        name: "podcast_generate".to_string(),
        description: "Podcast generator".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "script_path": {"type": "string"}
            }
        }),
        spawn_only: true,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    // Plugin's work_dir mirrors the modern `runtime/session.rs`
    // path: `<workspace>/skill-output/`.
    let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(skill_output.clone());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "script_path": "skill-output/mofa-podcast/octos_intro_script.md",
        }))
        .unwrap();

    assert_eq!(
        rewritten["script_path"],
        script.to_string_lossy().to_string(),
        "script_path must resolve to <work_dir>/mofa-podcast/<file>.md, \
             NOT the doubled <work_dir>/skill-output/mofa-podcast/<file>.md"
    );
}

#[test]
fn rewrite_workspace_file_args_keeps_skill_output_prefix_when_work_dir_is_workspace_root() {
    // Symmetric guard for the legacy `session_actor.rs` path:
    // when `work_dir` IS the workspace root (not chrooted into
    // `skill-output/`), the LLM's `skill-output/<file>` path is
    // correct as-is and must resolve to
    // `<workspace>/skill-output/<file>` — NOT have its prefix
    // stripped.
    let workspace = tempfile::tempdir().unwrap();
    let podcast_dir = workspace.path().join("skill-output").join("mofa-podcast");
    std::fs::create_dir_all(&podcast_dir).unwrap();
    let script = podcast_dir.join("intro.md");
    std::fs::write(&script, b"# script").unwrap();

    let def = PluginToolDef {
        name: "podcast_generate".to_string(),
        description: "Podcast generator".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"script_path": {"type": "string"}}
        }),
        spawn_only: true,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(workspace.path().to_path_buf());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "script_path": "skill-output/mofa-podcast/intro.md",
        }))
        .unwrap();

    assert_eq!(
        rewritten["script_path"],
        script.to_string_lossy().to_string(),
    );
}

#[test]
fn strip_redundant_skill_output_prefix_rejects_parent_dir_escape() {
    // Codex BLOCKER fix (PR #1186 review): malicious input like
    // `skill-output/../secret.md` must NOT slip through the
    // `strip_redundant_skill_output_prefix` helper, AND the
    // unsafe-component guard inside `resolve_path_in_work_dir`
    // must skip the existence-check branches so the lexical
    // `work_dir.join(...)` fallback cannot escape the chrooted
    // `skill-output/` subdir.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // Bait file ABOVE the chroot — escape attempt would land here.
    let secret = workspace.path().join("secret.md");
    std::fs::write(&secret, b"SECRET").unwrap();

    // 1. The helper itself refuses the unsafe candidate.
    assert!(
        strip_redundant_skill_output_prefix("skill-output/../secret.md", &skill_output).is_none(),
        "stripped output of a `..`-containing raw path must be None"
    );

    // 2. resolve_path_in_work_dir must return None (so the
    //    existence-check branches are bypassed) — i.e. the EXISTENCE
    //    of the bait file must not drive the result.
    assert!(
        resolve_path_in_work_dir("skill-output/../secret.md", &skill_output).is_none(),
        "resolve_path_in_work_dir must return None for `..` escape, \
             NOT the existing bait file's resolved path"
    );

    // 3. End-to-end: codex round-3 fail-closed contract. The full
    //    resolver MUST return Err for any input carrying `..`. The
    //    prior behaviour (returning the raw string unchanged) was
    //    unsafe because the spawned plugin has
    //    `cmd.current_dir(skill_output)`, so when the plugin's own
    //    process opens `skill-output/../secret.md` (or worse, the
    //    raw `../secret.md`) the kernel resolves it relative to the
    //    chrooted work_dir and escapes. We must surface the
    //    rejection to the caller (which propagates a tool error
    //    envelope), NOT pass through.
    let err = resolve_plugin_input_path("skill-output/../secret.md", &skill_output)
        .expect_err("parent-dir escape must return Err, not pass-through");
    let msg = err.to_string();
    assert!(
        msg.contains("escapes plugin work dir"),
        "error message must explain why the path was rejected: {msg}"
    );
    // Defense in depth: even if a future refactor returns Ok, the
    // resolved string must never point at the bait file.
    let _ = secret; // suppress unused warning under future refactors
}

#[test]
fn resolve_plugin_input_path_returns_err_on_raw_parent_dir() {
    // Codex round-3 BLOCKER fix (PR #1186 review): the round-2 fix
    // returned the raw string unchanged when `..` was present, but
    // the plugin process is spawned with
    // `cmd.current_dir(work_dir)`. Passing `../secret.md` through
    // unchanged lets the plugin itself open the path relative to
    // the chrooted work_dir and escape. The resolver must FAIL
    // CLOSED with an explicit error so the call site
    // (`rewrite_workspace_file_args` -> `prepare_effective_args`
    // -> `execute`) short-circuits the spawn and surfaces the
    // rejection to the LLM as a tool error envelope.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // Bait file ABOVE the chroot.
    let secret = workspace.path().join("secret.md");
    std::fs::write(&secret, b"SECRET").unwrap();

    // 1. The low-level helper must still return None — the
    //    fail-closed guarantee at the entry of the lexical-join
    //    helpers is unchanged.
    assert!(
        resolve_path_in_work_dir("../secret.md", &skill_output).is_none(),
        "resolve_path_in_work_dir must return None for raw `..` escape"
    );

    // 2. Top-level resolver returns Err — NOT a pass-through
    //    string — for every raw form of `..` escape.
    for raw in ["../secret.md", "..", "foo/../bar", "a/b/../../c"] {
        let err = resolve_plugin_input_path(raw, &skill_output).expect_err(&format!(
            "raw `..` input {raw:?} must return Err, not a pass-through string"
        ));
        let msg = err.to_string();
        assert!(
            msg.contains(raw),
            "error must echo the rejected raw path so the LLM sees what was refused: {msg}",
        );
        assert!(
            msg.contains("escapes plugin work dir"),
            "error must explain the rejection reason: {msg}",
        );
    }

    // 3. End-to-end via `rewrite_workspace_file_args`: any raw
    //    `..` path on a workspace-file key (`script_path`,
    //    `input`, `audio_path`, `file_path`, `video_path`,
    //    `text_path`) must abort the rewrite. The caller
    //    (execute()) returns a tool error envelope instead of
    //    spawning the plugin with a poisoned arg.
    let def = PluginToolDef {
        name: "podcast_generate".to_string(),
        description: "Podcast generator".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"script_path": {"type": "string"}}
        }),
        spawn_only: true,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(skill_output.clone());
    let rewrite_err = tool
        .rewrite_workspace_file_args(&json!({
            "script_path": "../secret.md",
        }))
        .expect_err("rewrite must propagate the resolver Err");
    assert!(
        rewrite_err.to_string().contains("../secret.md"),
        "rewrite error must echo the offending path: {rewrite_err}"
    );

    // Defense in depth: even if a future refactor accidentally
    // returns Ok, the resolved string must never point at the
    // bait file.
    let _ = secret;
}

#[test]
fn rewrite_workspace_file_args_rejects_raw_parent_dir_on_output_keys() {
    // Codex round-4 BLOCKER fix (PR #1186 review): the round-3
    // fail-closed Err contract on input-path keys (`audio_path`,
    // `file_path`, `input`, `script_path`, `video_path`,
    // `text_path`) did NOT cover OUTPUT-path keys. The
    // `out` / `slide_dir` keys are routed through
    // `absolutize_path_in_work_dir`, which previously did a naive
    // lexical join. A `{"out":"../sneaky"}` or
    // `{"slide_dir":"../escape"}` therefore produced a
    // `<work_dir>/../sneaky` string that the plugin (spawned with
    // `cmd.current_dir(work_dir)`) WOULD then write to — escaping
    // the chroot. Round-4 extends the fail-closed Err contract to
    // these keys: `absolutize_path_in_work_dir` now returns
    // `Result<String, Err>` and rejects raw `..` at the entry, and
    // `rewrite_workspace_file_args` `?`-propagates so the
    // `execute()` boundary returns a tool error envelope BEFORE
    // spawn.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // Bait file above the chroot (would-be victim of escape).
    let bait = workspace.path().join("sneaky");
    std::fs::write(&bait, b"BAIT").unwrap();

    let def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "Slides generator".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "out": {"type": "string"},
                "slide_dir": {"type": "string"}
            }
        }),
        spawn_only: true,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("mofa-slides".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(skill_output.clone());

    // Every output-key + escape-pattern combination MUST produce
    // an Err propagated through `rewrite_workspace_file_args`.
    let cases = [
        ("out", "../sneaky"),
        ("slide_dir", "../escape"),
        ("out", "subdir/../../../etc/passwd"),
        // Trailing-`..` escape: legacy `work_dir.join("..")` would
        // resolve to the parent of `work_dir`.
        ("slide_dir", ".."),
        // Mid-path `..` escape: `work_dir.join("a/../../escape")`
        // would resolve one level above `work_dir`.
        ("out", "a/../../escape"),
    ];

    for (key, raw) in cases {
        let err = tool
            .rewrite_workspace_file_args(&json!({ key: raw }))
            .expect_err(&format!(
                "output-path key {key:?} with raw `..` input {raw:?} \
                     must propagate Err from absolutize_path_in_work_dir"
            ));
        let msg = err.to_string();
        assert!(
            msg.contains(raw),
            "error for {key:?}={raw:?} must echo the offending path: {msg}",
        );
        assert!(
            msg.contains("escapes plugin work dir"),
            "error for {key:?}={raw:?} must explain rejection reason: {msg}",
        );
    }

    // Defense in depth: the underlying helper itself must Err so a
    // future refactor that bypasses `rewrite_workspace_file_args`
    // (e.g. a new call site) still fails closed.
    let helper_err = absolutize_path_in_work_dir("../sneaky", &skill_output)
        .expect_err("absolutize must Err on raw `..`");
    assert!(
        helper_err.to_string().contains("escapes plugin work dir"),
        "helper error must explain rejection: {helper_err}"
    );

    // Safe inputs still flow through unchanged (regression guard):
    // a relative path without `..` produces a lexical join, and an
    // absolute path is passed verbatim. Without this, a refactor
    // could over-zealously reject legitimate output args.
    let safe = absolutize_path_in_work_dir("sub/dir/out.toml", &skill_output)
        .expect("safe relative path must succeed");
    assert_eq!(
        safe,
        skill_output.join("sub/dir/out.toml").to_string_lossy()
    );
    // A platform-absolute path is passed through verbatim (the sandbox /
    // scope check is the next gate). `/tmp/...` is not absolute on Windows
    // (`Path::is_absolute` needs a drive or UNC root), so use a drive-absolute
    // path there.
    #[cfg(windows)]
    let abs_in = "C:\\tmp\\explicit-out.toml";
    #[cfg(not(windows))]
    let abs_in = "/tmp/explicit-out.toml";
    let abs_out = absolutize_path_in_work_dir(abs_in, &skill_output)
        .expect("absolute path must succeed (sandbox is the next gate)");
    assert_eq!(abs_out, abs_in);

    let _ = bait;
}

#[test]
fn rewrite_workspace_file_args_recovers_workspace_root_script_for_podcast_generate() {
    // NEW-02 mini5 soak fix: when `write_file` lands the podcast
    // script at the workspace ROOT (because write_file's base_dir is
    // `<workspace>/`, not `<workspace>/skill-output/`), but the
    // plugin's `work_dir` is chrooted to
    // `<workspace>/skill-output/`, the script lives one level ABOVE
    // the chroot. Before this fix the resolver only probed inside
    // `work_dir`, so #1186's shared resolver returned a non-existent
    // path and the plugin spawn failed with `os error 2`.
    //
    // The rescue branch in `resolve_plugin_input_path` now probes
    // `work_dir.parent()` (the workspace root) for the basename
    // when `work_dir` ends in `skill-output`. Both raw forms the
    // LLM tends to emit MUST recover:
    //   * `script.md`                — bare basename
    //   * `skill-output/script.md`   — with the redundant prefix
    //     (mirrors write_file's workspace-root resolution)
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // Mimic write_file landing the script at the workspace ROOT.
    let script = workspace.path().join("script.md");
    std::fs::write(&script, b"# podcast script\n").unwrap();

    // Form 1: bare basename. The legacy resolver would return
    // `<skill-output>/script.md` (lexical join, doesn't exist) or
    // fall through to the basename-scan inside the chroot (also
    // empty). Rescue must promote the workspace-root candidate.
    let resolved_bare = resolve_plugin_input_path("script.md", &skill_output)
        .expect("bare basename must resolve to workspace-root script");
    assert_eq!(
        std::path::Path::new(&resolved_bare),
        &script,
        "bare basename rescue must point at the workspace-root script",
    );

    // Form 2: `skill-output/`-prefixed path. The first strip-probe
    // would yield `script.md`, which doesn't exist inside the
    // chroot either. Same rescue must apply.
    let resolved_prefixed = resolve_plugin_input_path("skill-output/script.md", &skill_output)
        .expect("prefixed path must resolve to workspace-root script");
    assert_eq!(
        std::path::Path::new(&resolved_prefixed),
        &script,
        "prefixed-form rescue must point at the workspace-root script",
    );

    // End-to-end via `rewrite_workspace_file_args`: the
    // `script_path` key must be rewritten to the absolute
    // workspace-root path so the plugin spawn opens the right file.
    let def = PluginToolDef {
        name: "podcast_generate".to_string(),
        description: "Podcast generator".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"script_path": {"type": "string"}}
        }),
        spawn_only: true,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(skill_output.clone());
    let rewritten = tool
        .rewrite_workspace_file_args(&json!({"script_path": "script.md"}))
        .expect("rewrite must succeed for workspace-root script");
    let rewritten_path = rewritten
        .get("script_path")
        .and_then(|v| v.as_str())
        .expect("script_path must remain a string after rewrite");
    assert_eq!(
        std::path::Path::new(rewritten_path),
        &script,
        "rewrite must point script_path at the workspace-root file",
    );

    // SECURITY GUARANTEE — #1186 fail-closed contract for raw `..`
    // must STILL hold. The rescue is bounded to `work_dir.parent()`
    // via `Path::file_name()` (basename only), so directory
    // components in the raw path are discarded. But the entry
    // guard rejects `..` long before we get there, and that
    // behaviour must NOT regress.
    let traversal_err = resolve_plugin_input_path("../../etc/passwd", &skill_output)
        .expect_err("raw `..` traversal must still fail-closed per #1186");
    let msg = traversal_err.to_string();
    assert!(
        msg.contains("../../etc/passwd"),
        "error must echo the rejected raw path: {msg}",
    );
    assert!(
        msg.contains("escapes plugin work dir"),
        "error must explain rejection reason: {msg}",
    );
}

#[cfg(unix)]
#[test]
fn workspace_root_rescue_rejects_symlink_to_outside_workspace() {
    // Codex review on PR #1189 (BLOCKER): the rescue branch
    // originally used `candidate.exists()`, which FOLLOWS symlinks.
    // A `<workspace>/script.md -> /etc/passwd` symlink would have
    // satisfied `exists()` and the plugin would have received an
    // absolute path to a host file outside the workspace. The
    // hardened branch uses `symlink_metadata` + `is_file()` so
    // symlinks are caught before the path is handed off.
    //
    // This regression test creates a symlink at the workspace root
    // pointing at `/etc/passwd` and asserts the resolver REFUSES
    // to promote it via the rescue branch. The expected behaviour
    // is that the resolver falls through to the deeper fallbacks
    // (which either succeed inside `skill-output/` or, on absence,
    // produce the lexical-join string — neither lands on the
    // outside-workspace file).
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // Symlink at the workspace ROOT pointing OUTSIDE the workspace.
    let bait = workspace.path().join("script.md");
    std::os::unix::fs::symlink("/etc/passwd", &bait).unwrap();
    // Sanity check: the symlink target exists on a real host
    // (so `exists()` would have succeeded), but it MUST NOT drive
    // the rescue.
    assert!(
        std::fs::symlink_metadata(&bait)
            .unwrap()
            .file_type()
            .is_symlink(),
        "test setup: bait must be a symlink"
    );

    let resolved = resolve_plugin_input_path("script.md", &skill_output)
        .expect("resolver still returns a path (lexical fallback) but NOT the bait");
    let resolved_path = std::path::Path::new(&resolved);
    // Critical: the resolved path MUST NOT be the workspace-root
    // symlink. Any value pointing at `/etc/passwd` (directly or
    // through the symlink) would be a security failure.
    assert_ne!(
        resolved_path, bait,
        "rescue must not return the symlinked workspace-root path"
    );
    assert!(
        !resolved_path.starts_with("/etc"),
        "resolved path must not escape into /etc: {resolved}",
    );
    // The expected fall-through is `<skill_output>/script.md`
    // (lexical join from the basename scan inside work_dir, or the
    // final absolutize step). That path doesn't exist either — but
    // it's CONTAINED to the chroot, so the plugin will hit a clean
    // os error 2 instead of reading the bait.
    assert!(
        resolved_path.starts_with(&skill_output) || resolved_path.starts_with(workspace.path()),
        "resolver must stay within the workspace: {resolved}",
    );
    // Defense in depth.
    let _ = bait;
}

#[test]
fn subdir_rescue_resolves_workspace_relative_script() {
    // #1377 slides fix: a SUBDIR-prefixed input (`slides/<deck>/script.js`)
    // written at the workspace ROOT must resolve when work_dir is
    // chrooted to `<workspace>/skill-output/`. Before the fix only the
    // basename was probed at the root, so this missed and the agent fell
    // back to the overwrite-prone inline-array mode.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    let deck_dir = workspace.path().join("slides").join("deck");
    std::fs::create_dir_all(&deck_dir).unwrap();
    let script = deck_dir.join("script.js");
    std::fs::write(&script, "module.exports = []").unwrap();

    let resolved = resolve_plugin_input_path("slides/deck/script.js", &skill_output)
        .expect("subdir-prefixed workspace-relative input must resolve");
    assert_eq!(
        std::fs::canonicalize(&resolved).unwrap(),
        std::fs::canonicalize(&script).unwrap(),
        "must resolve to the real workspace-root script",
    );
}

#[cfg(unix)]
#[test]
fn subdir_rescue_rejects_symlinked_ancestor_escape() {
    // Codex round-1 P1: the full-path rescue carries SUBDIR components,
    // so a symlinked ANCESTOR (`<workspace>/slides -> /etc`) could let
    // `slides/passwd` escape — `symlink_metadata` only checks the final
    // component. The canonical-containment guard must reject it.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // `<workspace>/slides` is a symlink to /etc (an ancestor of the input).
    let bait_dir = workspace.path().join("slides");
    std::os::unix::fs::symlink("/etc", &bait_dir).unwrap();

    let resolved = resolve_plugin_input_path("slides/passwd", &skill_output)
        .expect("resolver still returns a (contained) fallback path, not the escape");
    let resolved_path = std::path::Path::new(&resolved);
    assert!(
        !resolved_path.starts_with("/etc"),
        "rescue must not resolve through a symlinked ancestor into /etc: {resolved}",
    );
    // Falls through to a contained path (skill-output basename join, or
    // the lexical fallback) — never the escaped /etc/passwd.
    assert!(
        resolved_path.starts_with(workspace.path()),
        "resolved path must stay within the workspace: {resolved}",
    );
    let _ = bait_dir;
}

#[cfg(unix)]
#[test]
fn workspace_root_rescue_rejects_symlink_to_inside_workspace() {
    // Defense-in-depth: codex review #1189 noted that symlinks
    // pointing INSIDE the workspace should also be rejected by
    // the rescue branch. The check is symlink-target-agnostic —
    // any symlink at the rescue candidate path fails because
    // `is_file()` (on `symlink_metadata`) returns false for the
    // symlink itself, regardless of what it points at. This test
    // pins that behaviour so a future refactor doesn't loosen
    // the predicate to e.g. follow symlinks within the workspace
    // and re-introduce TOCTOU swap risk.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // Real file lives inside skill-output.
    let real = skill_output.join("real.md");
    std::fs::write(&real, b"real").unwrap();
    // Symlink at the workspace root pointing INSIDE the workspace.
    let aliased = workspace.path().join("script.md");
    std::os::unix::fs::symlink(&real, &aliased).unwrap();

    let resolved = resolve_plugin_input_path("script.md", &skill_output)
        .expect("resolver still returns a path via fallback");
    let resolved_path = std::path::Path::new(&resolved);
    assert_ne!(
        resolved_path, aliased,
        "rescue must NOT return the workspace-root symlink even when it points inside",
    );
}

#[test]
fn workspace_root_rescue_rejects_directory_at_workspace_root() {
    // The rescue branch must also reject non-file candidates
    // (directories, sockets, FIFOs). A directory at
    // `<workspace>/script.md` should NOT satisfy the rescue —
    // plugins expect to read a file, and handing them a directory
    // path is at best confusing, at worst exploitable.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // Create a DIRECTORY (not a file) at the rescue candidate.
    let dir_at_root = workspace.path().join("script.md");
    std::fs::create_dir(&dir_at_root).unwrap();

    let resolved = resolve_plugin_input_path("script.md", &skill_output)
        .expect("resolver still returns a path via fallback");
    let resolved_path = std::path::Path::new(&resolved);
    // The directory MUST NOT be promoted by the rescue branch.
    assert_ne!(
        resolved_path, dir_at_root,
        "rescue must not return a directory at the workspace root"
    );
}

#[test]
fn strip_redundant_skill_output_prefix_rejects_absolute_paths() {
    // Codex BLOCKER fix (PR #1186 review): absolute paths (e.g.
    // `/etc/passwd`) must not be accepted by the strip helper or
    // by the existence-check branches of
    // `resolve_path_in_work_dir`. The shared `resolve_tool_path`
    // resolver in the upstream caller already rejects them, but
    // the legacy fallback chain must not silently accept them
    // either: the EXISTENCE of the absolute file on disk must
    // never drive the result.
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();

    assert!(
        strip_redundant_skill_output_prefix("/etc/passwd", &skill_output).is_none(),
        "absolute paths must not be stripped"
    );

    // Critical security guarantee: the existence-check branches of
    // resolve_path_in_work_dir are skipped for absolute paths.
    // Returns None (so the caller falls back to a lexical join or
    // the absolutize fallback — both safe in that the sandbox
    // gates the real subprocess), NOT Some("/etc/passwd").
    assert!(
        resolve_path_in_work_dir("/etc/passwd", &skill_output).is_none(),
        "resolve_path_in_work_dir must return None for absolute paths, \
             NOT the raw path because the file exists on disk"
    );
}

#[test]
fn rewrite_workspace_file_args_rewrites_video_path_and_text_path() {
    // Codex MAJOR fix (PR #1186 review): mofa-frame uses
    // `video_path` and the (unpublished) mofa-videolizer uses
    // `text_path` for their input args. Both must be subject to
    // the same workspace-relative rewrite as `audio_path` /
    // `file_path` / `script_path`.
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("clip.mp4");
    let text = dir.path().join("transcript.txt");
    std::fs::write(&video, b"mp4").unwrap();
    std::fs::write(&text, b"hello").unwrap();

    let def = PluginToolDef {
        name: "frame_tool".to_string(),
        description: "mofa-frame style tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "video_path": {"type": "string"},
                "text_path": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("mofa-frame".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(dir.path().to_path_buf());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "video_path": "clip.mp4",
            "text_path": "transcript.txt",
        }))
        .unwrap();

    assert_eq!(
        rewritten["video_path"],
        video.to_string_lossy().to_string(),
        "mofa-frame video_path must be rewritten to absolute work_dir path"
    );
    assert_eq!(
        rewritten["text_path"],
        text.to_string_lossy().to_string(),
        "mofa-videolizer text_path must be rewritten to absolute work_dir path"
    );
}

#[test]
fn rewrite_workspace_file_args_keeps_mofa_style_as_name() {
    let dir = tempfile::tempdir().unwrap();
    let styles = dir.path().join("styles");
    std::fs::create_dir_all(&styles).unwrap();
    let style = styles.join("cyberpunk-neon.toml");
    std::fs::write(&style, b"[meta]\nname='Cyberpunk'\n").unwrap();

    let def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "Slides tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "style": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(dir.path().to_path_buf());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "style": "cyberpunk-neon"
        }))
        .unwrap();

    assert_eq!(rewritten["style"], "cyberpunk-neon");
}

#[test]
fn rewrite_workspace_file_args_strips_mofa_style_toml_paths_to_name() {
    let dir = tempfile::tempdir().unwrap();
    let styles = dir.path().join("styles");
    std::fs::create_dir_all(&styles).unwrap();
    let style = styles.join("cyberpunk-neon.toml");
    std::fs::write(&style, b"[meta]\nname='Cyberpunk'\n").unwrap();

    let def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "Slides tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "style": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(dir.path().to_path_buf());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "style": style.to_string_lossy().to_string()
        }))
        .unwrap();

    assert_eq!(rewritten["style"], "cyberpunk-neon");
}

#[test]
fn rewrite_workspace_file_args_strips_repeated_mofa_style_toml_suffixes() {
    let dir = tempfile::tempdir().unwrap();

    let def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "Slides tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "style": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(dir.path().to_path_buf());

    let rewritten = tool
        .rewrite_workspace_file_args(&json!({
            "style": "/tmp/styles/nb-pro.toml.toml"
        }))
        .unwrap();

    assert_eq!(rewritten["style"], "nb-pro");
}

#[test]
fn prepare_effective_args_injects_attachment_defaults() {
    let def = PluginToolDef {
        name: "voice_tool".to_string(),
        description: "Voice tool".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "audio_path": {"type": "string"},
                "file_path": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));
    let ctx = ToolContext {
        tool_id: "tool-1".to_string(),
        reporter: Arc::new(SilentReporter),
        harness_event_sink: None,
        attachment_paths: vec![
            "/workspace/voice.ogg".to_string(),
            "/workspace/report.pdf".to_string(),
        ],
        audio_attachment_paths: vec!["/workspace/voice.ogg".to_string()],
        file_attachment_paths: vec!["/workspace/report.pdf".to_string()],
        ..ToolContext::zero()
    };

    let prepared = tool.prepare_effective_args(&json!({}), Some(&ctx)).unwrap();

    assert_eq!(prepared["audio_path"], "/workspace/voice.ogg");
    assert_eq!(prepared["file_path"], "/workspace/report.pdf");
}

fn deep_search_def_with_opt_in() -> PluginToolDef {
    PluginToolDef {
        name: "search".to_string(),
        description: "Deep research".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "synthesis_config": {"type": "object"}
            },
            "x-octos-host-config-keys": ["synthesis_config"]
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    }
}

fn notebook_source_def_with_workspace_root_opt_in() -> PluginToolDef {
    PluginToolDef {
        name: "source_import".to_string(),
        description: "Import notebook source".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "workspace_root": {"type": "string"}
            },
            "x-octos-host-config-keys": ["workspace_root"]
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    }
}

fn full_synthesis_config() -> SynthesisConfig {
    SynthesisConfig {
        endpoint: "https://api.deepseek.com/v1".to_string(),
        api_key: "sk-host-injected".to_string(),
        model: "deepseek-chat".to_string(),
        provider: "deepseek".to_string(),
    }
}

#[test]
fn synthesis_config_is_complete_only_when_all_fields_populated() {
    let cfg = full_synthesis_config();
    assert!(cfg.is_complete());

    let mut partial = cfg.clone();
    partial.api_key.clear();
    assert!(!partial.is_complete());

    let mut partial = cfg.clone();
    partial.endpoint.clear();
    assert!(!partial.is_complete());
}

#[test]
fn prepare_effective_args_injects_synthesis_config_when_opted_in() {
    let tool = PluginTool::new(
        "deep-search".into(),
        deep_search_def_with_opt_in(),
        PathBuf::from("/bin/true"),
    )
    .with_synthesis_config(full_synthesis_config());

    let prepared = tool
        .prepare_effective_args(&json!({"query": "AI policy"}), None)
        .unwrap();
    let cfg = &prepared["synthesis_config"];
    assert_eq!(cfg["endpoint"], "https://api.deepseek.com/v1");
    assert_eq!(cfg["api_key"], "sk-host-injected");
    assert_eq!(cfg["model"], "deepseek-chat");
    assert_eq!(cfg["provider"], "deepseek");
}

#[test]
fn prepare_effective_args_skips_synthesis_config_when_manifest_does_not_opt_in() {
    // Same tool but without the x-octos-host-config-keys extension.
    let mut def = deep_search_def_with_opt_in();
    def.input_schema = json!({
        "type": "object",
        "properties": {"query": {"type": "string"}}
    });
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
        .with_synthesis_config(full_synthesis_config());

    let prepared = tool
        .prepare_effective_args(&json!({"query": "AI policy"}), None)
        .unwrap();
    assert!(
        prepared.get("synthesis_config").is_none(),
        "tools without opt-in must not receive synthesis_config: {prepared}",
    );
}

#[test]
fn prepare_effective_args_skips_synthesis_config_when_host_did_not_set_one() {
    let tool = PluginTool::new(
        "deep-search".into(),
        deep_search_def_with_opt_in(),
        PathBuf::from("/bin/true"),
    );

    let prepared = tool
        .prepare_effective_args(&json!({"query": "AI policy"}), None)
        .unwrap();
    assert!(prepared.get("synthesis_config").is_none());
}

#[test]
fn prepare_effective_args_skips_synthesis_config_when_partial() {
    let mut cfg = full_synthesis_config();
    cfg.api_key.clear(); // Partial → fall through to env path.
    let tool = PluginTool::new(
        "deep-search".into(),
        deep_search_def_with_opt_in(),
        PathBuf::from("/bin/true"),
    )
    .with_synthesis_config(cfg);

    let prepared = tool
        .prepare_effective_args(&json!({"query": "AI policy"}), None)
        .unwrap();
    assert!(prepared.get("synthesis_config").is_none());
}

#[test]
fn prepare_effective_args_does_not_overwrite_explicit_synthesis_config() {
    // Defense in depth: if a caller already set synthesis_config (e.g. a
    // unit test or a future LLM-controlled override), don't silently
    // replace it.
    let tool = PluginTool::new(
        "deep-search".into(),
        deep_search_def_with_opt_in(),
        PathBuf::from("/bin/true"),
    )
    .with_synthesis_config(full_synthesis_config());

    let prepared = tool
        .prepare_effective_args(
            &json!({
                "query": "AI policy",
                "synthesis_config": {"api_key": "caller-supplied"}
            }),
            None,
        )
        .unwrap();
    assert_eq!(prepared["synthesis_config"]["api_key"], "caller-supplied");
    assert!(
        prepared["synthesis_config"].get("endpoint").is_none(),
        "host config must not be merged into caller-supplied synthesis_config",
    );
}

#[test]
fn prepare_effective_args_injects_workspace_root_when_opted_in() {
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
    let ctx = ToolContext {
        session_scope: Some(Arc::new(scope)),
        ..ToolContext::zero()
    };
    let tool = PluginTool::new(
        "mofa-notebook-source".into(),
        notebook_source_def_with_workspace_root_opt_in(),
        PathBuf::from("/bin/true"),
    )
    .with_work_dir(skill_output);

    let prepared = tool
        .prepare_effective_args(&json!({"path": "docs/report.md"}), Some(&ctx))
        .unwrap();

    assert_eq!(
        prepared["workspace_root"],
        workspace.path().to_string_lossy().as_ref()
    );
}

#[test]
fn prepare_effective_args_skips_workspace_root_when_manifest_does_not_opt_in() {
    let workspace = tempfile::tempdir().unwrap();
    let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
    let ctx = ToolContext {
        session_scope: Some(Arc::new(scope)),
        ..ToolContext::zero()
    };
    let mut def = notebook_source_def_with_workspace_root_opt_in();
    def.input_schema = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}}
    });
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

    let prepared = tool
        .prepare_effective_args(&json!({"path": "docs/report.md"}), Some(&ctx))
        .unwrap();

    assert!(prepared.get("workspace_root").is_none());
}

/// `workspace_root` is host-owned metadata: when the manifest opts in,
/// the host-computed value ALWAYS wins over a caller-supplied one, so a
/// spoofed tool call can't point the plugin outside the session root.
#[test]
fn prepare_effective_args_overwrites_caller_supplied_workspace_root_with_host_value() {
    let workspace = tempfile::tempdir().unwrap();
    let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
    let ctx = ToolContext {
        session_scope: Some(Arc::new(scope)),
        ..ToolContext::zero()
    };
    let tool = PluginTool::new(
        "mofa-notebook-source".into(),
        notebook_source_def_with_workspace_root_opt_in(),
        PathBuf::from("/bin/true"),
    );

    let prepared = tool
        .prepare_effective_args(
            &json!({
                "path": "docs/report.md",
                "workspace_root": "/caller/workspace"
            }),
            Some(&ctx),
        )
        .unwrap();

    assert_eq!(
        prepared["workspace_root"],
        workspace.path().to_string_lossy().as_ref(),
        "host-computed workspace_root must override the caller-supplied value"
    );
    assert_ne!(prepared["workspace_root"], "/caller/workspace");
}

/// When the manifest opts in but the host cannot compute a workspace
/// root (no scope, no work dir), a caller-supplied value is STRIPPED
/// rather than preserved — host-owned metadata never passes through
/// from the caller.
#[test]
fn prepare_effective_args_strips_caller_supplied_workspace_root_without_host_value() {
    let tool = PluginTool::new(
        "mofa-notebook-source".into(),
        notebook_source_def_with_workspace_root_opt_in(),
        PathBuf::from("/bin/true"),
    );

    let prepared = tool
        .prepare_effective_args(
            &json!({
                "path": "docs/report.md",
                "workspace_root": "/caller/workspace"
            }),
            None,
        )
        .unwrap();

    assert!(
        prepared.get("workspace_root").is_none(),
        "caller-supplied workspace_root must be stripped when the host has no computed value"
    );
}

#[test]
fn prepare_effective_args_infers_workspace_root_from_skill_output_without_scope() {
    let workspace = tempfile::tempdir().unwrap();
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    let tool = PluginTool::new(
        "mofa-notebook-source".into(),
        notebook_source_def_with_workspace_root_opt_in(),
        PathBuf::from("/bin/true"),
    )
    .with_work_dir(skill_output);

    let prepared = tool
        .prepare_effective_args(&json!({"path": "docs/report.md"}), None)
        .unwrap();

    assert_eq!(
        prepared["workspace_root"],
        workspace.path().to_string_lossy().as_ref()
    );
}

/// Write a script to a file and make it executable, with fsync to avoid ETXTBSY
/// on Linux overlayfs (Docker containers).
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_spawns_subprocess_and_captures_output() {
    // Create a temp script that reads stdin and writes structured JSON to stdout.
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT\necho '{\"output\": \"got: '\"$INPUT\"'\", \"success\": true}'\n",
    );

    let def = make_tool_def("echo_tool", "echoes input");
    let tool = PluginTool::new("test-plugin".into(), def, script_path)
        .with_timeout(Duration::from_secs(5));

    let args = json!({"msg": "hello"});
    let result = tool.execute(&args).await.expect("execute should succeed");

    assert!(result.success);
    assert!(
        result.output.contains("got:"),
        "output should contain echoed input, got: {}",
        result.output
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_structured_progress_event_updates_task_supervisor() {
    use crate::task_supervisor::TaskSupervisor;
    use serde_json::json;

    let dir = tempfile::tempdir().expect("create temp dir");
    let supervisor = Arc::new(TaskSupervisor::new());
    let task_id = supervisor.register("structured_tool", "call-1", Some("api:session"));
    supervisor.mark_running(&task_id);

    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\ncat >/dev/null\nprintf '{\"schema\":\"octos.harness.event.v1\",\"kind\":\"progress\",\"session_id\":\"%s\",\"task_id\":\"%s\",\"workflow\":\"deep_research\",\"phase\":\"fetching_sources\",\"message\":\"Fetching source 3/12\",\"progress\":0.42}\\n' \"$OCTOS_SESSION_ID\" \"$OCTOS_TASK_ID\" >> \"$OCTOS_EVENT_SINK\"\nprintf '{\"output\":\"ok\",\"success\":true}'\n",
    );

    let def = make_tool_def("structured_tool", "writes harness events");
    let tool = PluginTool::new("test-plugin".into(), def, script_path)
        .with_timeout(Duration::from_secs(5));

    let sink = crate::harness_events::HarnessEventSink::new(
        supervisor.clone(),
        task_id.clone(),
        "api:session",
    )
    .expect("create sink");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    supervisor.set_on_change(move |task| {
        let _ = tx.send(task.clone());
    });

    let ctx = ToolContext {
        tool_id: "tool-1".to_string(),
        reporter: Arc::new(SilentReporter),
        harness_event_sink: Some(sink.path().display().to_string()),
        attachment_paths: vec![],
        audio_attachment_paths: vec![],
        file_attachment_paths: vec![],
        ..ToolContext::zero()
    };

    let result = crate::tools::TOOL_CTX
        .scope(ctx, tool.execute(&json!({})))
        .await
        .expect("tool execution should succeed");
    assert!(result.success);

    let updated = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("callback should fire")
        .expect("task snapshot should be sent");

    let detail: serde_json::Value =
        serde_json::from_str(updated.runtime_detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["workflow_kind"], "deep_research");
    assert_eq!(detail["current_phase"], "fetching_sources");
    assert_eq!(detail["progress_message"], "Fetching source 3/12");
    assert_eq!(updated.status, crate::task_supervisor::TaskStatus::Running);
    assert_eq!(
        updated.lifecycle_state(),
        crate::task_supervisor::TaskLifecycleState::Running
    );

    let task = supervisor.get_task(&task_id).expect("task missing");
    let task_detail: serde_json::Value =
        serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
    assert_eq!(task_detail["current_phase"], "fetching_sources");
    assert_eq!(task_detail["progress_message"], "Fetching source 3/12");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_does_not_expose_secret_extra_env_without_tool_allowlist() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\nVALUE=${OPENAI_API_KEY:-missing}\necho '{\"output\":\"'\"$VALUE\"'\",\"success\":true}'\n",
    );

    let def = make_tool_def("env_tool", "prints env");
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_extra_env(vec![(
            "OPENAI_API_KEY".into(),
            "sk-octos-plugin-regression".into(),
        )])
        .with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("should succeed");

    assert!(result.success);
    assert_eq!(result.output, "missing");
    assert!(!result.output.contains("sk-octos-plugin-regression"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_exposes_secret_extra_env_with_tool_allowlist() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\nVALUE=${OPENAI_API_KEY:-missing}\necho '{\"output\":\"'\"$VALUE\"'\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("env_tool", "prints env");
    def.env.push("OPENAI_API_KEY".into());
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_extra_env(vec![(
            "OPENAI_API_KEY".into(),
            "sk-octos-plugin-allowed".into(),
        )])
        .with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("should succeed");

    assert!(result.success);
    assert_eq!(result.output, "sk-octos-plugin-allowed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_fallback_on_non_json_stdout() {
    // Script that outputs plain text (not JSON).
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(&script_path, "#!/bin/sh\necho 'plain text output'\n");

    let def = make_tool_def("plain_tool", "plain output");
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("should succeed");

    assert!(result.success);
    assert!(result.output.contains("plain text output"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_fallback_detects_generated_pptx_as_file_to_send() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let output_rel = "slides/demo/output/deck.pptx";
    let output_abs = dir.path().join(output_rel);
    std::fs::create_dir_all(output_abs.parent().unwrap()).unwrap();
    std::fs::write(&output_abs, b"fake pptx").unwrap();

    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho 'Generated PPTX: slides/demo/output/deck.pptx'\n",
    );

    let def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "slides output".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "out": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_work_dir(dir.path().to_path_buf())
        .with_timeout(Duration::from_secs(5));

    let result = tool
        .execute(&json!({"out": output_rel}))
        .await
        .expect("should succeed");

    assert!(result.success);
    assert_eq!(result.file_modified.as_deref(), Some(output_abs.as_path()));
    assert_eq!(result.files_to_send, vec![output_abs]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_fallback_waits_briefly_for_generated_pptx_to_appear() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let output_rel = "slides/demo/output/deck.pptx";
    let output_abs = dir.path().join(output_rel);

    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nnohup sh -c 'sleep 0.2; mkdir -p slides/demo/output; printf fake > slides/demo/output/deck.pptx' >/dev/null 2>&1 &\necho 'Generated PPTX: slides/demo/output/deck.pptx'\n",
    );

    let def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "slides output".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "out": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_work_dir(dir.path().to_path_buf())
        .with_timeout(Duration::from_secs(5));

    let result = tool
        .execute(&json!({"out": output_rel}))
        .await
        .expect("should succeed");

    assert!(result.success);
    assert_eq!(result.file_modified.as_deref(), Some(output_abs.as_path()));
    assert_eq!(result.files_to_send, vec![output_abs.clone()]);
    assert!(
        output_abs.exists(),
        "generated deck should appear after fallback wait"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_fallback_skips_missing_generated_pptx() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let output_rel = "slides/demo/output/deck.pptx";

    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho 'Generated PPTX: slides/demo/output/deck.pptx'\n",
    );

    let def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "slides output".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "out": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_work_dir(dir.path().to_path_buf())
        .with_timeout(Duration::from_secs(5));

    let result = tool
        .execute(&json!({"out": output_rel}))
        .await
        .expect("should succeed");

    assert!(result.success);
    assert_eq!(result.file_modified, None);
    assert!(result.files_to_send.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_timeout_returns_error() {
    // Skip in Docker containers where pid/process management can cause hangs.
    // This test passes on macOS and bare-metal Linux.
    if std::path::Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup")
            .map(|s| s.contains("docker") || s.contains("kubepods"))
            .unwrap_or(false)
    {
        eprintln!("skipping execute_timeout_returns_error: container detected");
        return;
    }

    // Script that sleeps longer than the timeout.
    // multi_thread needed because execute() spawns reader tasks that must run
    // concurrently with the timeout future.
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(&script_path, "#!/bin/sh\nsleep 60\n");

    let def = make_tool_def("slow_tool", "too slow");
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(1));

    match tool.execute(&json!({})).await {
        Err(e) => assert!(
            e.to_string().contains("timed out"),
            "expected timeout error, got: {e}"
        ),
        Ok(_) => panic!("expected timeout error, but execute succeeded"),
    }
}

/// A process is considered "alive" for the orphan check if `ps` reports a
/// non-zombie state for the pid. A reaped or zombie process is dead.
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let state = String::from_utf8_lossy(&o.stdout);
            let state = state.trim();
            // Empty output -> no such process (reaped). A leading 'Z' is a
            // zombie -> the process is dead, awaiting reap.
            !state.is_empty() && !state.starts_with('Z')
        }
        // Non-zero exit (or spawn error) -> no such pid.
        _ => false,
    }
}

/// Cancellation-safety regression (codex review of 7c3e5eac): the registry
/// timeout (`execute_with_context`) wraps the tool future in
/// `tokio::time::timeout`, which DROPS the future on elapse. A `PluginTool`
/// spawns a child subprocess that is owned by that future. If the dropped
/// future does not kill the child, the plugin subprocess is ORPHANED and
/// keeps mutating state — trading a hang for a runaway process.
///
/// This simulates the registry path by racing `execute()` against a SHORT
/// timeout and dropping the future on elapse, then asserting the spawned
/// child PID is actually dead (not just that the future returned).
///
/// RED on HEAD: without `kill_on_drop(true)` the `sleep` child survives the
/// drop and `pid_is_alive` stays true.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn dropped_execute_future_kills_child_no_orphan() {
    if std::path::Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup")
            .map(|s| s.contains("docker") || s.contains("kubepods"))
            .unwrap_or(false)
    {
        eprintln!("skipping dropped_execute_future_kills_child_no_orphan: container detected");
        return;
    }

    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    let pidfile = dir.path().join("child.pid");
    // `exec sleep` so the recorded pid IS the long-running process (no
    // intermediate shell). Ignores stdin entirely, so a hang here also
    // exercises the pre-kill stdin-write path. Sleeps far longer than the
    // test timeout so it cannot exit on its own.
    write_test_script(
        &script_path,
        &format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 600\n",
            pidfile.display()
        ),
    );

    let def = make_tool_def("hang_tool", "ignores stdin and sleeps");
    // A long internal plugin timeout so it is NOT the plugin's own kill
    // branch that saves us — only dropping the future can kill the child.
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(600));

    // Simulate the registry dispatch boundary: wrap the tool future in a
    // short timeout. On elapse the future is dropped (its `Child` with it).
    // Use a 3s window (generous even under heavy parallel test load) so the
    // child reliably runs `echo $$ > pidfile` and reaches `sleep` BEFORE
    // the timeout drops the future — otherwise we could not observe the
    // pid to assert on. The cancellation-safety property is unaffected by
    // the window length.
    let args = json!({});
    let fut = tool.execute(&args);
    let res = tokio::time::timeout(Duration::from_secs(3), fut).await;
    assert!(
        res.is_err(),
        "expected the short registry-style timeout to elapse (future dropped)"
    );
    // `res` (the dropped future) is gone here — the `Child` was owned by it.

    // The child should have written its pid before sleeping. It had the
    // full 3s window above to do so.
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .expect("child should have recorded its pid before sleeping");

    // Poll: the drop-kill + tokio reaper should make the child dead. Allow
    // a brief window for SIGKILL delivery + reap.
    let mut alive_after = true;
    for _ in 0..100 {
        if !pid_is_alive(pid) {
            alive_after = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Best-effort cleanup so a RED run does not leak the orphan.
    if alive_after {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }

    assert!(
        !alive_after,
        "plugin child (pid {pid}) was ORPHANED after the execute future was dropped — \
             not cancellation-safe"
    );
}

/// Cancellation-safety regression, GRANDCHILD edition (codex re-review of
/// af3597ab — Gap 3's "limits must degrade, never leak"): `kill_on_drop(true)`
/// reaps only the DIRECT plugin child on future-drop. A plugin that spawns
/// its OWN children (a worker, `sleep 600 &`, etc.) leaves those
/// GRANDCHILDREN running after a registry-timeout cancellation unless the
/// plugin was placed in its own process group and the whole group is killed.
///
/// This races `execute()` against a short timeout and drops the future, then
/// asserts BOTH the direct child pid AND the spawned grandchild pid are dead.
///
/// RED on HEAD `af3597ab`: no `process_group(0)` before spawn + drop-time
/// group-kill, so the grandchild survives the drop and `pid_is_alive` stays
/// true for it. GREEN after the spawn is put in its own group and a Drop
/// guard SIGKILLs the group.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn dropped_execute_future_kills_child_and_grandchild_no_orphan() {
    if std::path::Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup")
            .map(|s| s.contains("docker") || s.contains("kubepods"))
            .unwrap_or(false)
    {
        eprintln!(
            "skipping dropped_execute_future_kills_child_and_grandchild_no_orphan: container detected"
        );
        return;
    }

    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    let pidfile = dir.path().join("pids");
    // The plugin spawns a background GRANDCHILD (`sleep 600 &`), records the
    // grandchild's pid AND its own pid into the pidfile, then `exec sleep`s
    // so the recorded `$$` is the long-running direct child (no intermediate
    // shell). Both pids must be reaped on cancellation. Ignores stdin.
    write_test_script(
        &script_path,
        &format!(
            "#!/bin/sh\nsleep 600 &\necho $! >> '{0}'\necho $$ >> '{0}'\nexec sleep 600\n",
            pidfile.display()
        ),
    );

    let def = make_tool_def(
        "hang_tool_with_grandchild",
        "spawns a grandchild and sleeps",
    );
    // Long internal plugin timeout: only dropping the future (and the
    // group-kill it triggers) can reap the tree, not the plugin's own
    // kill branch.
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(600));

    // Simulate the registry dispatch boundary: a short timeout drops the
    // future (and its `Child`) on elapse. 3s window so the script reliably
    // records both pids before the drop.
    let args = json!({});
    let fut = tool.execute(&args);
    let res = tokio::time::timeout(Duration::from_secs(3), fut).await;
    assert!(
        res.is_err(),
        "expected the short registry-style timeout to elapse (future dropped)"
    );

    // Read BOTH recorded pids (grandchild first, then direct child).
    let contents = {
        let mut last = String::new();
        for _ in 0..100 {
            last = std::fs::read_to_string(&pidfile).unwrap_or_default();
            if last.lines().filter(|l| !l.trim().is_empty()).count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        last
    };
    let pids: Vec<u32> = contents
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect();
    assert!(
        pids.len() >= 2,
        "expected the plugin to record both grandchild and child pids, got: {contents:?}"
    );
    let grandchild_pid = pids[0];
    let child_pid = pids[1];

    // Poll: the drop-time group-kill should reap BOTH the direct child and
    // the grandchild. Allow a brief window for SIGKILL delivery + reap.
    let mut child_alive = true;
    let mut grandchild_alive = true;
    for _ in 0..100 {
        child_alive = pid_is_alive(child_pid);
        grandchild_alive = pid_is_alive(grandchild_pid);
        if !child_alive && !grandchild_alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Best-effort cleanup so a RED run does not leak orphans.
    for pid in [grandchild_pid, child_pid] {
        if pid_is_alive(pid) {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }

    assert!(
        !child_alive,
        "plugin DIRECT child (pid {child_pid}) was ORPHANED after the execute future was dropped"
    );
    assert!(
        !grandchild_alive,
        "plugin GRANDCHILD (pid {grandchild_pid}) was ORPHANED after the execute future was \
             dropped — process tree leaked (kill_on_drop reaps only the direct child)"
    );
}

// -------------------------------------------------------------------
// Plugin protocol v2 stderr dispatch tests (W3.F2).
// -------------------------------------------------------------------

use crate::progress::ProgressReporter;
use std::sync::Mutex as StdMutex;

/// Captures every reported event so tests can assert on the ToolProgress
/// messages the v2 shim emits.
struct CapturingReporter {
    events: Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
}

impl ProgressReporter for CapturingReporter {
    fn report(&self, event: crate::progress::ProgressEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

fn make_capturing_ctx() -> (
    ToolContext,
    Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
) {
    let events = Arc::new(StdMutex::new(Vec::<crate::progress::ProgressEvent>::new()));
    let mut ctx = ToolContext::zero();
    ctx.tool_id = "tool-1".to_string();
    ctx.reporter = Arc::new(CapturingReporter {
        events: Arc::clone(&events),
    });
    (ctx, events)
}

fn last_progress_message(
    events: &Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
) -> Option<String> {
    events.lock().unwrap().last().and_then(|event| match event {
        crate::progress::ProgressEvent::ToolProgress { message, .. } => Some(message.clone()),
        _ => None,
    })
}

#[test]
fn v2_progress_event_renders_stage_and_message() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line(
        "deep-search",
        "search",
        Some(&ctx),
        r#"{"type":"progress","stage":"searching","message":"round 1/3","progress":0.25}"#,
    );
    let msg = last_progress_message(&events).expect("emitted progress");
    assert!(msg.contains("[searching]"), "expected stage badge: {msg}");
    assert!(msg.contains("25%"), "expected percent: {msg}");
    assert!(msg.contains("round 1/3"), "expected message: {msg}");
}

#[test]
fn v2_phase_event_renders_phase_label() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line(
        "deep-search",
        "search",
        Some(&ctx),
        r#"{"type":"phase","phase":"synthesizing","message":"calling LLM"}"#,
    );
    let msg = last_progress_message(&events).expect("emitted progress");
    assert!(msg.starts_with("[synthesizing]"), "got {msg}");
    assert!(msg.contains("calling LLM"), "got {msg}");
}

#[test]
fn v2_cost_event_renders_cost_summary() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line(
        "deep-search",
        "search",
        Some(&ctx),
        r#"{"type":"cost","provider":"deepseek","model":"deepseek-chat","tokens_in":1024,"tokens_out":256,"usd":0.0034}"#,
    );
    let msg = last_progress_message(&events).expect("emitted progress");
    assert!(msg.contains("[cost]"), "got {msg}");
    assert!(msg.contains("deepseek"), "got {msg}");
    assert!(msg.contains("in=1024"), "got {msg}");
    assert!(msg.contains("out=256"), "got {msg}");
    assert!(msg.contains("0.0034"), "got {msg}");
}

#[test]
fn v2_log_event_renders_level() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line(
        "deep-search",
        "search",
        Some(&ctx),
        r#"{"type":"log","level":"warn","message":"low disk"}"#,
    );
    let msg = last_progress_message(&events).expect("emitted progress");
    assert_eq!(msg, "[warn] low disk");
}

#[test]
fn v2_artifact_event_renders_kind_and_path() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line(
        "deep-search",
        "search",
        Some(&ctx),
        r#"{"type":"artifact","path":"/tmp/x.md","kind":"report","message":"final"}"#,
    );
    let msg = last_progress_message(&events).expect("emitted progress");
    assert!(msg.contains("[artifact:report]"), "got {msg}");
    assert!(msg.contains("/tmp/x.md"), "got {msg}");
}

#[test]
fn legacy_v1_text_passes_through_unchanged() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line(
        "old-plugin",
        "old_tool",
        Some(&ctx),
        "[deep_crawl] launched chrome on port 9222",
    );
    let msg = last_progress_message(&events).expect("emitted progress");
    assert_eq!(msg, "[deep_crawl] launched chrome on port 9222");
}

#[test]
fn legacy_starting_with_bracket_does_not_lose_data() {
    // Plugins emitting `[1/3] Searching ...` style text must still flow
    // through unchanged — they are not JSON, the shim must not eat them.
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line(
        "deep-search",
        "search",
        Some(&ctx),
        "[1/3] Searching: \"foo\"",
    );
    let msg = last_progress_message(&events).expect("emitted progress");
    assert_eq!(msg, "[1/3] Searching: \"foo\"");
}

#[test]
fn malformed_json_falls_back_to_legacy() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line(
        "p",
        "t",
        Some(&ctx),
        r#"{"type":"progress""#, // truncated, parse fails
    );
    let msg = last_progress_message(&events).expect("emitted progress");
    // Falls back to the raw line (trimmed).
    assert_eq!(msg, r#"{"type":"progress""#);
}

#[test]
fn empty_line_emits_no_progress() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line("p", "t", Some(&ctx), "");
    PluginTool::dispatch_stderr_line("p", "t", Some(&ctx), "   \r\n");
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn unknown_event_type_passes_raw_through() {
    let (ctx, events) = make_capturing_ctx();
    PluginTool::dispatch_stderr_line("p", "t", Some(&ctx), r#"{"type":"future_event","data":42}"#);
    let msg = last_progress_message(&events).expect("emitted progress");
    // The raw JSON is forwarded so the operator can still see it.
    assert!(msg.contains("future_event"), "got {msg}");
}

#[test]
fn dispatch_with_no_ctx_is_noop() {
    // No assertion — just confirm there's no panic. With no ctx the
    // shim cannot dispatch but it must not crash.
    PluginTool::dispatch_stderr_line(
        "p",
        "t",
        None,
        r#"{"type":"progress","stage":"init","message":"go"}"#,
    );
}

#[test]
fn cost_event_writes_to_harness_sink() {
    let dir = tempfile::tempdir().unwrap();
    let sink_path = dir.path().join("events.ndjson");

    // Wire up a sink context so record_cost_event has a session+task to
    // attribute against.
    let ctx_path = sink_path.display().to_string();
    crate::harness_events::attach_event_sink_context(
        ctx_path.clone(),
        crate::harness_events::HarnessEventSinkContext {
            session_id: "session-1".to_string(),
            task_id: "task-1".to_string(),
        },
    );

    let mut ctx = ToolContext::zero();
    ctx.tool_id = "tool-1".to_string();
    ctx.harness_event_sink = Some(ctx_path.clone());

    PluginTool::dispatch_stderr_line(
        "deep-search",
        "search",
        Some(&ctx),
        r#"{"type":"cost","provider":"deepseek","model":"deepseek-chat","tokens_in":1024,"tokens_out":256,"usd":0.0034}"#,
    );

    let body = std::fs::read_to_string(&sink_path).expect("sink written");
    assert!(body.contains(r#""kind":"cost_attribution""#), "got: {body}");
    assert!(body.contains(r#""tokens_in":1024"#), "got: {body}");
    assert!(body.contains(r#""tokens_out":256"#), "got: {body}");
    assert!(body.contains(r#""cost_usd":0.0034"#), "got: {body}");
    assert!(body.contains(r#""contract_id":"plugin:deep-search:search""#));
    assert!(body.contains(r#""provider":"deepseek""#));

    // Cleanup the sink registration.
    crate::harness_events::detach_event_sink_context(&ctx_path);
}

// -------------------------------------------------------------------
// M6 req 4: env allowlist + risk approval enforcement tests
// -------------------------------------------------------------------

/// Manifest declares `env: ["FOO_ALLOWED_PLUGIN"]`. With strict gate
/// active, an extra_env entry that's NOT on the manifest list is
/// dropped — even though it isn't a secret name, the legacy gate
/// would forward it. Pin the new strict semantics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn strict_env_allowlist_drops_non_listed_extra_env() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\nA=${FOO_ALLOWED_PLUGIN:-missing}\nN=${FOO_BLOCKED_PLUGIN:-missing}\necho '{\"output\":\"a='\"$A\"';n='\"$N\"'\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("env_strict_tool", "prints env");
    def.env.push("FOO_ALLOWED_PLUGIN".into());
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_extra_env(vec![
            ("FOO_ALLOWED_PLUGIN".into(), "yes".into()),
            ("FOO_BLOCKED_PLUGIN".into(), "should_be_stripped".into()),
        ])
        .with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("should succeed");

    assert!(result.success);
    assert!(
        result.output.contains("a=yes"),
        "listed extra env should reach subprocess; got: {}",
        result.output
    );
    assert!(
        result.output.contains("n=missing"),
        "non-listed extra env must be stripped under strict allowlist; got: {}",
        result.output
    );
}

/// When the manifest declares an empty `env` list, legacy semantics
/// apply: non-secret extra_env entries pass through unfiltered. This
/// pins the no-regression contract: skills that don't declare `env`
/// see no behavior change from this PR.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn empty_env_allowlist_keeps_legacy_extra_env_passthrough() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    // Use a name that isn't flagged as secret-like (no token match
    // for SECRET/TOKEN/KEY/PASSWORD/etc).
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\nVALUE=${MY_BASE_URL:-missing}\necho '{\"output\":\"'\"$VALUE\"'\",\"success\":true}'\n",
    );

    let def = make_tool_def("legacy_env_tool", "prints env");
    // No `env` allowlist declared → empty list → legacy gate.
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_extra_env(vec![("MY_BASE_URL".into(), "passes_through".into())])
        .with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("should succeed");

    assert!(result.success);
    assert!(
        result.output.contains("passes_through"),
        "non-secret extra_env should pass through under legacy gate; got: {}",
        result.output
    );
}

/// Strict allowlist must still permit runtime essentials like PATH
/// even if they aren't listed in the manifest, otherwise the
/// subprocess can't find binaries it needs (sh, etc.). PATH is
/// inherited from the parent process, not injected via extra_env.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn strict_env_allowlist_retains_path() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\nVALUE=${PATH:-missing}\nif [ \"$VALUE\" = \"missing\" ]; then echo '{\"output\":\"NO_PATH\",\"success\":true}'; else echo '{\"output\":\"HAS_PATH\",\"success\":true}'; fi\n",
    );

    let mut def = make_tool_def("path_tool", "prints PATH");
    def.env.push("FOO_ALLOWED_PLUGIN".into());
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("should succeed");
    assert!(result.success);
    assert!(
        result.output.contains("HAS_PATH"),
        "PATH must be retained under strict allowlist; got: {}",
        result.output
    );
}

// ---- risk approval gate ----

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use std::sync::Mutex;

#[cfg(unix)]
use crate::tools::ToolApprovalRequester;

#[cfg(unix)]
struct RecordingRequester {
    decision: ToolApprovalDecision,
    last: Arc<Mutex<Option<ToolApprovalRequest>>>,
}

#[cfg(unix)]
impl RecordingRequester {
    fn new(decision: ToolApprovalDecision) -> (Arc<Self>, Arc<Mutex<Option<ToolApprovalRequest>>>) {
        let last = Arc::new(Mutex::new(None));
        let r = Arc::new(Self {
            decision,
            last: last.clone(),
        });
        (r, last)
    }
}

#[cfg(unix)]
#[async_trait]
impl ToolApprovalRequester for RecordingRequester {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        *self.last.lock().unwrap() = Some(request);
        self.decision
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn high_risk_plugin_tool_requests_approval() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\necho '{\"output\":\"ran\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("danger_tool", "danger");
    def.risk = Some("high".into());
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Approve);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let result = TOOL_APPROVAL_CTX
        .scope(requester_arc, tool.execute(&json!({})))
        .await
        .expect("execute should succeed");

    assert!(result.success);
    assert_eq!(result.output, "ran");
    let req = last
        .lock()
        .unwrap()
        .clone()
        .expect("approval was requested");
    assert_eq!(req.tool_name, "danger_tool");
    assert!(req.title.contains("high"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn high_risk_plugin_tool_denied_returns_deny_message() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"output\":\"should_not_run\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("danger_tool_deny", "danger");
    def.risk = Some("critical".into());
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let (requester, _last) = RecordingRequester::new(ToolApprovalDecision::Deny);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let result = TOOL_APPROVAL_CTX
        .scope(requester_arc, tool.execute(&json!({})))
        .await
        .expect("execute returns Ok with deny message");

    assert!(!result.success, "denied call must report failure");
    assert!(
        result.output.contains("denied"),
        "deny message should be returned; got: {}",
        result.output
    );
    assert!(!result.output.contains("should_not_run"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn low_risk_plugin_tool_does_not_request_approval() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"output\":\"ran_without_prompt\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("safe_tool", "safe");
    def.risk = Some("low".into());
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Deny);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let result = TOOL_APPROVAL_CTX
        .scope(requester_arc, tool.execute(&json!({})))
        .await
        .expect("execute should succeed");

    assert!(result.success);
    assert_eq!(result.output, "ran_without_prompt");
    assert!(
        last.lock().unwrap().is_none(),
        "approval must not be requested for low risk"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn unspecified_risk_plugin_tool_does_not_request_approval() {
    // Default behavior — pinning that skills without `risk` declared
    // continue to run without ever prompting (no breakage).
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"output\":\"unprompted\",\"success\":true}'\n",
    );

    let def = make_tool_def("plain_tool", "plain");
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Deny);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let result = TOOL_APPROVAL_CTX
        .scope(requester_arc, tool.execute(&json!({})))
        .await
        .expect("execute should succeed");

    assert!(result.success);
    assert_eq!(result.output, "unprompted");
    assert!(last.lock().unwrap().is_none());
}

// ---- risk gate honors ApprovalPolicy (yolo GAP #2) ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn should_deny_high_risk_plugin_without_prompt_when_approval_policy_never() {
    // yolo GAP #2: the manifest risk gate previously ignored
    // `ApprovalPolicy`, so a `never`/full-access session still got an
    // approval prompt. Parity with shell.rs's fail-closed
    // "approval_policy is never": under `Never` (and NOT a dangerous
    // auto-allow context) a high-risk plugin must be DENIED without ever
    // issuing an approval request.
    use crate::policy::ApprovalPolicy;

    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"output\":\"should_not_run\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("danger_never", "danger");
    def.risk = Some("high".into());
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_timeout(Duration::from_secs(5))
        .with_approval_policy(ApprovalPolicy::Never);

    let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Approve);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let result = TOOL_APPROVAL_CTX
        .scope(requester_arc, tool.execute(&json!({})))
        .await
        .expect("execute returns Ok with deny message");

    assert!(!result.success, "never policy must fail the high-risk call");
    assert!(
        result.output.contains("approval_policy is never"),
        "deny message should cite the never policy; got: {}",
        result.output
    );
    assert!(!result.output.contains("should_not_run"));
    assert!(
        last.lock().unwrap().is_none(),
        "no approval request may be issued under approval_policy=never"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn should_auto_allow_high_risk_plugin_without_prompt_when_danger_full_access() {
    // yolo GAP #2: a DangerFullAccess / AllowAll context auto-approves the
    // risk gate (parity with shell's AllowAllPolicy under danger) — the
    // plugin runs without a prompt even though its `approval_policy` is
    // `Never` (which danger implies).
    use crate::policy::ApprovalPolicy;

    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\necho '{\"output\":\"ran_under_yolo\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("danger_yolo", "danger");
    def.risk = Some("critical".into());
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_timeout(Duration::from_secs(5))
        .with_approval_policy(ApprovalPolicy::Never)
        .with_auto_approve_high_risk(true);

    let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Deny);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let result = TOOL_APPROVAL_CTX
        .scope(requester_arc, tool.execute(&json!({})))
        .await
        .expect("execute should succeed");

    assert!(
        result.success,
        "danger full access must auto-allow the risk gate"
    );
    assert_eq!(result.output, "ran_under_yolo");
    assert!(
        last.lock().unwrap().is_none(),
        "no approval request may be issued under a danger auto-allow context"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn should_request_approval_for_high_risk_plugin_when_approval_policy_ask() {
    // Regression pin: the default Ask policy still routes a high-risk
    // plugin through the interactive approval bridge.
    use crate::policy::ApprovalPolicy;

    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\necho '{\"output\":\"ran\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("danger_ask", "danger");
    def.risk = Some("high".into());
    let tool = PluginTool::new("p".into(), def, script_path)
        .with_timeout(Duration::from_secs(5))
        .with_approval_policy(ApprovalPolicy::Ask);

    let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Approve);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let result = TOOL_APPROVAL_CTX
        .scope(requester_arc, tool.execute(&json!({})))
        .await
        .expect("execute should succeed");

    assert!(result.success);
    assert_eq!(result.output, "ran");
    assert!(
        last.lock().unwrap().is_some(),
        "Ask policy must still request approval for a high-risk plugin"
    );
}

#[test]
fn should_thread_session_approval_context_into_plugin_tools_on_rebind() {
    // yolo GAP #2 wiring: `apply_permissions_to_plugin_tools` (invoked by
    // `rebind_cwd_with_permissions`) must replace each plugin tool with a
    // copy carrying the session's approval context, so a plugin registered
    // at profile-build time inherits the per-session `ApprovalPolicy`.
    use crate::policy::{ApprovalPolicy, EffectivePermissions, PermissionProfile, RuntimeMode};
    use crate::tools::ToolRegistry;

    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    // Never executed — this test only checks approval-context threading
    // through the registry rebind, so the placeholder just needs to exist
    // on disk. `write_test_script` (real shebang + chmod +x) is Unix-only;
    // an empty file keeps this test running on Windows too.
    std::fs::write(&script_path, "").unwrap();
    let mut def = make_tool_def("risky", "risky");
    def.risk = Some("high".into());

    // A plugin tool starts with the interactive default (Ask, no auto-allow).
    let mut registry = ToolRegistry::new();
    registry.register(PluginTool::new("p".into(), def, script_path));
    {
        let base = registry.get("risky").expect("plugin registered");
        let pt = base
            .as_any()
            .downcast_ref::<PluginTool>()
            .expect("is a PluginTool");
        assert_eq!(pt.approval_policy(), ApprovalPolicy::Ask);
        assert!(!pt.auto_approve_high_risk());
    }

    // A `never` workspace session: the risk gate must fail closed.
    let never = EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never);
    registry.apply_permissions_to_plugin_tools(never);
    {
        let pt_arc = registry.get("risky").unwrap();
        let pt = pt_arc.as_any().downcast_ref::<PluginTool>().unwrap();
        assert_eq!(pt.approval_policy(), ApprovalPolicy::Never);
        assert!(
            !pt.auto_approve_high_risk(),
            "workspace-write never is NOT an auto-allow context"
        );
    }

    // A DangerFullAccess ("yolo") session: the risk gate auto-allows.
    let danger =
        EffectivePermissions::for_runtime(PermissionProfile::DangerFullAccess, RuntimeMode::Solo)
            .expect("solo danger");
    registry.apply_permissions_to_plugin_tools(danger);
    {
        let pt_arc = registry.get("risky").unwrap();
        let pt = pt_arc.as_any().downcast_ref::<PluginTool>().unwrap();
        assert!(
            pt.auto_approve_high_risk(),
            "DangerFullAccess must set the auto-allow flag on plugin tools"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn high_risk_without_approval_bridge_denies_safely() {
    // Mirrors shell.rs behavior: if there's no interactive bridge,
    // a high-risk plugin tool must NOT silently run.
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"output\":\"should_not_run\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("danger_tool_no_bridge", "danger");
    def.risk = Some("HIGH".into());
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    // No TOOL_APPROVAL_CTX scoped → try_with returns Err → deny.
    let result = tool
        .execute(&json!({}))
        .await
        .expect("returns Ok with deny");
    assert!(!result.success);
    assert!(result.output.contains("denied"));
    assert!(!result.output.contains("should_not_run"));
}

#[test]
fn concurrency_class_trims_whitespace_and_returns_exclusive() {
    // Codex review #1 regression test: `"exclusive "` (trailing
    // whitespace) previously silently downgraded to Safe. After the
    // trim added at the parse site, it must classify as Exclusive.
    let mut def = make_tool_def("excl_tool", "exclusive");
    def.concurrency_class = Some("exclusive ".to_string());
    let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"));
    let class = tool.concurrency_class();
    assert!(matches!(class, crate::tools::ConcurrencyClass::Exclusive));
}

#[test]
fn plugin_unknown_concurrency_class_falls_back_to_exclusive() {
    // Issue #718 follow-up: align with MCP's
    // `McpServerConfig::resolved_concurrency_class`. The previous
    // behavior was fail-open (unknown → Safe), which silently
    // permitted parallel writes when a manifest author typoed
    // `"exclusve"`. After the fix, unknown literals fail-closed to
    // Exclusive — same behavior as MCP — so a typo still serialises
    // execution.
    let mut def = make_tool_def("excl_tool", "exclusive");
    def.concurrency_class = Some("highly-exclusive".to_string());
    let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"));
    assert!(matches!(
        tool.concurrency_class(),
        crate::tools::ConcurrencyClass::Exclusive,
    ));

    // The exact typo called out in #718.
    let mut typo_def = make_tool_def("typo_tool", "exclusive");
    typo_def.concurrency_class = Some("exclusve".to_string());
    let typo_tool = PluginTool::new("p".into(), typo_def, PathBuf::from("/bin/echo"));
    assert!(matches!(
        typo_tool.concurrency_class(),
        crate::tools::ConcurrencyClass::Exclusive,
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn unknown_risk_literal_does_not_force_approval() {
    // medium / weird literals fall through to "no enforced gate"
    // (semantics ambiguous; documented as Tier-2/3 follow-up).
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"output\":\"ran\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("medium_tool", "medium");
    def.risk = Some("medium".into());
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Deny);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let result = TOOL_APPROVAL_CTX
        .scope(requester_arc, tool.execute(&json!({})))
        .await
        .expect("execute should succeed");

    assert!(result.success);
    assert_eq!(result.output, "ran");
    assert!(last.lock().unwrap().is_none());
}

// -------------------------------------------------------------------
// Wave-3b: spawn_only stdout envelope extension — `named_outputs`.
// -------------------------------------------------------------------

#[test]
fn parse_named_outputs_returns_none_when_field_absent() {
    // Tool that doesn't emit named_outputs should parse cleanly to None
    // so existing spawn_only callers stay byte-identical.
    let envelope = json!({"success": true, "output": "ok"});
    let parsed = parse_named_outputs(envelope.get("named_outputs")).unwrap();
    assert!(parsed.is_none());
}

#[test]
fn parse_named_outputs_returns_none_when_field_is_null() {
    let envelope = json!({"named_outputs": null});
    let parsed = parse_named_outputs(envelope.get("named_outputs")).unwrap();
    assert!(parsed.is_none());
}

#[test]
fn parse_named_outputs_returns_none_when_object_is_empty() {
    let envelope = json!({"named_outputs": {}});
    let parsed = parse_named_outputs(envelope.get("named_outputs")).unwrap();
    assert!(parsed.is_none());
}

#[test]
fn parse_named_outputs_maps_string_values() {
    let envelope = json!({
        "named_outputs": {
            "deploy_url": "https://example.com/site",
            "repo": "octos/site",
        }
    });
    let parsed = parse_named_outputs(envelope.get("named_outputs"))
        .unwrap()
        .expect("expected Some(map)");
    assert_eq!(
        parsed.get("deploy_url").map(String::as_str),
        Some("https://example.com/site")
    );
    assert_eq!(parsed.get("repo").map(String::as_str), Some("octos/site"));
}

#[test]
fn parse_named_outputs_rejects_non_object_payload() {
    let envelope = json!({"named_outputs": ["a", "b"]});
    let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
    assert!(err.contains("must be a JSON object"), "{err}");
}

#[test]
fn parse_named_outputs_rejects_non_string_value() {
    // v1: nested JSON not supported. Numbers, bools, arrays, objects
    // must surface as errors so the contract layer sees a typed
    // failure rather than silently dropping the field.
    let envelope = json!({
        "named_outputs": {"deploy_count": 42}
    });
    let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
    assert!(err.contains("must be a string"), "{err}");
    assert!(err.contains("deploy_count"), "{err}");
}

#[test]
fn parse_named_outputs_rejects_key_starting_with_digit() {
    let envelope = json!({"named_outputs": {"1deploy": "x"}});
    let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
    assert!(err.contains("required shape"), "{err}");
}

#[test]
fn parse_named_outputs_rejects_uppercase_key() {
    let envelope = json!({"named_outputs": {"DeployUrl": "x"}});
    let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
    assert!(err.contains("required shape"), "{err}");
}

#[test]
fn parse_named_outputs_rejects_key_with_hyphen() {
    let envelope = json!({"named_outputs": {"deploy-url": "x"}});
    let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
    assert!(err.contains("required shape"), "{err}");
}

#[test]
fn parse_named_outputs_rejects_empty_key() {
    let envelope = json!({"named_outputs": {"": "x"}});
    let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
    assert!(err.contains("required shape"), "{err}");
}

#[test]
fn parse_named_outputs_accepts_underscore_and_digits_after_first_char() {
    let envelope = json!({"named_outputs": {"deploy_url_v2": "x", "out1": "y"}});
    let parsed = parse_named_outputs(envelope.get("named_outputs"))
        .unwrap()
        .expect("expected map");
    assert_eq!(parsed.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_with_named_outputs_threads_field_into_tool_result() {
    // End-to-end: plugin emits {"named_outputs": {...}} on stdout, the
    // PluginTool wrapper forwards it through ToolResult so the
    // spawn_only contract path can read it.
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"success\":true,\"output\":\"deployed\",\"named_outputs\":{\"deploy_url\":\"http://example.com/site\"}}'\n",
    );

    let def = make_tool_def("publish_tool", "publish");
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("execute should ok");
    assert!(result.success);
    assert_eq!(result.output, "deployed");
    let named = result.named_outputs.expect("named_outputs should be set");
    assert_eq!(
        named.get("deploy_url").map(String::as_str),
        Some("http://example.com/site")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_with_malformed_named_outputs_returns_failure() {
    // A plugin emitting a non-string value in named_outputs must
    // surface as a typed failure so the contract layer rejects it.
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"success\":true,\"output\":\"ok\",\"named_outputs\":{\"count\":42}}'\n",
    );

    let def = make_tool_def("bad_tool", "emits bad named outputs");
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("execute should ok");
    assert!(!result.success);
    assert!(
        result.output.contains("named_outputs") || result.output.contains("must be a string"),
        "unexpected output: {}",
        result.output
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn execute_without_named_outputs_leaves_tool_result_none() {
    // Backward compat: legacy plugins that don't emit named_outputs
    // must continue to produce ToolResult.named_outputs = None.
    let dir = tempfile::tempdir().expect("create temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\necho '{\"success\":true,\"output\":\"done\"}'\n",
    );

    let def = make_tool_def("legacy_tool", "legacy");
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let result = tool.execute(&json!({})).await.expect("execute should ok");
    assert!(result.success);
    assert!(result.named_outputs.is_none());
}

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

fn multi_tenant_scope_at(
    data: &std::path::Path,
    tenant: &str,
    session: &str,
    shared_zones: Vec<std::path::PathBuf>,
) -> SessionScope {
    SessionScope::multi_tenant(
        data.to_path_buf(),
        tenant.into(),
        session.into(),
        shared_zones,
    )
    .expect("build multi-tenant scope")
}

#[cfg(unix)]
fn ctx_with_scope(scope: SessionScope) -> ToolContext {
    let mut ctx = ToolContext::zero();
    ctx.session_scope = Some(Arc::new(scope));
    ctx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn plugin_uses_scope_workspace_when_present() {
    // Phase 2-B contract: when a `SessionScope` is threaded via the
    // `ToolContext` AND `self.work_dir` is `None` (no registry
    // rebind happened, so the scope is the source of truth), the
    // plugin spawns with `OCTOS_WORK_DIR = scope.workspace()`. The
    // workspace dir is created on the fly so
    // `SessionScope::multi_tenant`'s no-create-on-construction
    // promise still holds and the spawner takes care of it.
    //
    // The "self.work_dir wins when set" path (the hinted-workspace
    // case that codex P1 flagged) is pinned separately by
    // `plugin_prefers_registry_rebound_work_dir_over_scope` below.
    let data = tempfile::tempdir().expect("data dir");
    // Use a session id that has not been created yet — Phase 2-B
    // must `create_dir_all(scope.workspace())` before spawn.
    let scope = multi_tenant_scope_at(data.path(), "dspfac", "web-phase2b", vec![]);
    let session_workspace = scope.workspace().to_path_buf();
    assert!(
        !session_workspace.exists(),
        "test fixture sanity: workspace must not pre-exist"
    );

    // The executable lives outside the scope workspace because the
    // test cannot pre-create the scope's session dir without
    // defeating the assertion below. Both dirs must exist before
    // `write_test_script` so we use an unrelated tempdir for the
    // binary.
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let script_path = bin_dir.path().join("script.sh");
    // Script echoes its CWD via `pwd` inside the JSON envelope so
    // the test can inspect it.
    write_test_script(
        &script_path,
        "#!/bin/sh\nDIR=$(pwd)\nprintf '{\"output\":\"%s\",\"success\":true}' \"$DIR\"\n",
    );

    let def = make_tool_def("scope_cwd", "echo CWD");
    // Crucially: NO `.with_work_dir(...)`. The scope is the only
    // source of truth.
    let tool =
        PluginTool::new("plug".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let ctx = ctx_with_scope(scope);
    let result = crate::tools::TOOL_CTX
        .scope(ctx, tool.execute(&json!({})))
        .await
        .expect("execute should succeed");

    assert!(result.success, "scope-aware execute should succeed");
    assert!(
        session_workspace.exists(),
        "Phase 2-B must create scope.workspace() before spawn"
    );
    // macOS prefixes tempdirs with `/private`, so canonicalise both
    // sides before comparing (the shell's `pwd` resolves symlinks).
    let actual =
        std::fs::canonicalize(result.output.trim()).expect("CWD echoed by plugin should resolve");
    let expected = std::fs::canonicalize(&session_workspace)
        .expect("scope workspace should resolve after create_dir_all");
    assert_eq!(
        actual, expected,
        "plugin CWD must equal scope.workspace() when self.work_dir is None"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn high_risk_plugin_approval_cwd_reflects_scope_workspace() {
    // Codex P3 pin (Phase 2-B): the approval prompt's `cwd` field
    // must reflect the directory the plugin will ACTUALLY run in,
    // not the construction-time `self.work_dir`. In a scope-only
    // wiring (no registry rebind), that's `scope.workspace()` —
    // before this fix the prompt would have shown `None` (or the
    // bogus construction work_dir), so users approving a
    // high/critical-risk plugin would see the wrong directory.
    let data = tempfile::tempdir().expect("data dir");
    let scope = multi_tenant_scope_at(data.path(), "dspfac", "web-approval", vec![]);
    let scope_workspace = scope.workspace().to_path_buf();

    // Place the binary outside the scope (we need it on disk so
    // `write_test_script` works) and DO NOT pass it via
    // `with_work_dir` — scope-only wiring.
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let script_path = bin_dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nread INPUT || true\necho '{\"output\":\"ran\",\"success\":true}'\n",
    );

    let mut def = make_tool_def("approval_cwd_tool", "danger");
    def.risk = Some("high".into());
    let tool = PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

    let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Approve);
    let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

    let ctx = ctx_with_scope(scope);
    let _ = crate::tools::TOOL_CTX
        .scope(
            ctx,
            TOOL_APPROVAL_CTX.scope(requester_arc, tool.execute(&json!({}))),
        )
        .await
        .expect("execute should succeed");

    let req = last
        .lock()
        .unwrap()
        .clone()
        .expect("approval was requested");
    let cwd = req
        .cwd
        .as_deref()
        .expect("approval cwd must be Some when scope is present");
    assert_eq!(
        std::path::Path::new(cwd),
        &scope_workspace,
        "approval cwd MUST reflect the effective work dir (scope.workspace() when scope-only)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn plugin_rescues_workspace_root_under_hinted_skill_output_rebind() {
    // Codex round-5 P1 pin (Phase 2-B): real hinted bootstrap
    // rebinds `self.work_dir = <hint>/skill-output`. The
    // workspace-root rescue (LLM passes `script_path: "script.md"`
    // when the file is at `<hint>/script.md`) must still resolve.
    // Round-4 rooted the ad-hoc scope at `wd` directly, which
    // surrendered that rescue; round-5 promotes the parent dir
    // when `wd` ends in `skill-output`.
    let hint = tempfile::tempdir().expect("hint");
    let skill_output = hint.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    // The script lives at the hinted workspace ROOT, not inside
    // `skill-output/` (mirrors the soak workflow where write_file
    // lands the script at the workspace root).
    let script = hint.path().join("script.md");
    std::fs::write(&script, b"# podcast script").unwrap();

    let scope_workspace = tempfile::tempdir().expect("scope workspace");
    let scope = solo_scope_at(scope_workspace.path());

    let bin = tempfile::tempdir().expect("bin");
    let bin_path = bin.path().join("script.sh");
    // The plugin echoes the script_path it received so the test
    // can inspect the rewrite.
    write_test_script(
        &bin_path,
        "#!/bin/sh\nINPUT=$(cat)\nVALUE=$(echo \"$INPUT\" | sed -n 's/.*\"script_path\":\"\\([^\"]*\\)\".*/\\1/p')\nprintf '{\"output\":\"%s\",\"success\":true}' \"$VALUE\"\n",
    );
    let tool = PluginTool::new("plug".into(), input_path_def("script_path"), bin_path)
        .with_work_dir(skill_output.clone())
        .with_timeout(Duration::from_secs(5));

    let ctx = ctx_with_scope(scope);
    let result = crate::tools::TOOL_CTX
        .scope(ctx, tool.execute(&json!({"script_path": "script.md"})))
        .await
        .expect("execute should succeed");

    assert!(result.success, "hinted workspace-root rescue must succeed");
    let echoed_path = result.output.trim();
    let echoed_canon = std::fs::canonicalize(echoed_path).expect("echoed path resolves");
    let expected_canon = std::fs::canonicalize(&script).expect("script resolves");
    assert_eq!(
        echoed_canon, expected_canon,
        "rescue must promote `<hint>/script.md` (the parent rescue), \
             NOT rewrite to `<hint>/skill-output/script.md`"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn plugin_refuses_absolute_escape_in_hinted_session() {
    // Codex round-4 P1 pin (Phase 2-B): when a scoped session has
    // a workspace_hint whose path falls outside the session scope
    // (the `SessionRuntime::bootstrap` reality today), the round-3
    // routing fell back to the legacy rewriter — which accepted
    // absolute paths anywhere on disk after `resolve_tool_path`
    // failed. The round-4 fix substitutes an AD-HOC solo scope
    // rooted at the hinted work_dir so the read/write boundary
    // still holds: an `audio_path: "/etc/passwd"` from a hinted
    // session MUST still Err.
    let scope_workspace = tempfile::tempdir().expect("scope workspace");
    let hinted_work_dir = tempfile::tempdir().expect("hinted work_dir");
    // Bait file outside the hinted work_dir.
    let bait_outside = tempfile::tempdir().expect("bait");
    let bait_path = bait_outside.path().join("escape.txt");
    std::fs::write(&bait_path, b"BAIT").unwrap();

    let scope = solo_scope_at(scope_workspace.path());
    // Mirror the hinted bootstrap shape: scope is at
    // `scope_workspace`, but `self.work_dir` is the hint
    // (`hinted_work_dir`), which is OUTSIDE `scope.workspace()`.
    // Round-3 would have routed this through the legacy rewriter
    // and accepted `/escape/path`. Round-4 substitutes an ad-hoc
    // solo scope rooted at the hint, so the absolute escape Errs.
    let bin = tempfile::tempdir().expect("bin");
    let script = bin.path().join("script.sh");
    write_test_script(
        &script,
        "#!/bin/sh\nread INPUT || true\necho '{\"output\":\"ran\",\"success\":true}'\n",
    );
    let tool = PluginTool::new("plug".into(), input_path_def("audio_path"), script.clone())
        .with_work_dir(hinted_work_dir.path().to_path_buf())
        .with_timeout(Duration::from_secs(5));

    let ctx = ctx_with_scope(scope);
    let bait_abs = bait_path.to_string_lossy().to_string();
    let result = crate::tools::TOOL_CTX
        .scope(ctx, tool.execute(&json!({"audio_path": bait_abs.clone()})))
        .await
        .expect("execute should return Ok with error envelope");

    assert!(
        !result.success,
        "absolute out-of-scope path under hint must produce a tool error envelope (success=false), got success=true output={}",
        result.output,
    );
    assert!(
        result.output.contains(&bait_abs),
        "tool error envelope must echo the rejected path: {}",
        result.output
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn plugin_prefers_registry_rebound_work_dir_over_scope() {
    // Codex P1 pin (Phase 2-B): when `SessionRuntime::bootstrap`
    // honours a `workspace_hint`, it calls
    // `tools.rebind_plugin_work_dirs(<hint>/skill-output)` so every
    // `PluginTool` clone carries `self.work_dir = <hint>/...`. The
    // `SessionScope` constructed alongside still derives its
    // workspace from `profile.data_dir` (= the un-hinted default),
    // so the two disagree. Phase 2-B MUST honour the registry
    // rebind (the hint is the source of truth in that wiring) and
    // NOT silently redirect the plugin to the empty default scope
    // workspace. This pin guards the regression codex flagged.
    let data = tempfile::tempdir().expect("data dir");
    // Multi-tenant scope: workspace lands at
    // `<data>/users/web-codex-p1/workspace`. We deliberately
    // never create it; the test asserts it stays absent because the
    // plugin runs in the registry-rebound dir instead.
    let scope = multi_tenant_scope_at(data.path(), "dspfac", "web-codex-p1", vec![]);
    let scope_workspace = scope.workspace().to_path_buf();
    assert!(
        !scope_workspace.exists(),
        "test fixture sanity: scope workspace must not pre-exist"
    );

    // Registry-rebound work_dir mirrors the hinted-workspace path.
    let hinted_work_dir = tempfile::tempdir().expect("hinted work dir");
    let script_path = hinted_work_dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nDIR=$(pwd)\nprintf '{\"output\":\"%s\",\"success\":true}' \"$DIR\"\n",
    );

    let def = make_tool_def("hint_cwd", "echo CWD");
    let tool = PluginTool::new("plug".into(), def, script_path)
        .with_work_dir(hinted_work_dir.path().to_path_buf())
        .with_timeout(Duration::from_secs(5));

    let ctx = ctx_with_scope(scope);
    let result = crate::tools::TOOL_CTX
        .scope(ctx, tool.execute(&json!({})))
        .await
        .expect("execute should succeed");

    assert!(result.success, "hinted execute should succeed");
    let actual =
        std::fs::canonicalize(result.output.trim()).expect("CWD echoed by plugin should resolve");
    let expected =
        std::fs::canonicalize(hinted_work_dir.path()).expect("hinted work_dir should resolve");
    assert_eq!(
        actual, expected,
        "registry-rebound self.work_dir MUST win over scope.workspace()"
    );
    // Defence in depth: the scope workspace must STILL be absent
    // because Phase 2-B did NOT redirect the spawn there.
    assert!(
        !scope_workspace.exists(),
        "scope workspace must NOT be created when self.work_dir wins"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn plugin_falls_back_to_self_work_dir_when_no_scope() {
    // Backward compat: legacy callers (no scope threaded) must keep
    // the construction-time `self.work_dir` as the plugin's CWD.
    let dir = tempfile::tempdir().expect("temp dir");
    let script_path = dir.path().join("script.sh");
    write_test_script(
        &script_path,
        "#!/bin/sh\nDIR=$(pwd)\nprintf '{\"output\":\"%s\",\"success\":true}' \"$DIR\"\n",
    );

    let def = make_tool_def("legacy_cwd", "echo CWD");
    let tool = PluginTool::new("plug".into(), def, script_path)
        .with_work_dir(dir.path().to_path_buf())
        .with_timeout(Duration::from_secs(5));

    // No scope threaded — execute via the default `TOOL_CTX::zero`
    // shape (the global TOOL_CTX::try_with returns Err so the
    // legacy path is taken).
    let result = tool
        .execute(&json!({}))
        .await
        .expect("execute should succeed");
    assert!(result.success);
    let actual =
        std::fs::canonicalize(result.output.trim()).expect("CWD echoed by plugin should resolve");
    let expected = std::fs::canonicalize(dir.path()).expect("construction dir should resolve");
    assert_eq!(
        actual, expected,
        "no-scope path must use construction-time work_dir"
    );
}

#[test]
fn plugin_refuses_out_of_scope_input_path() {
    // Phase 2-B: with scope, every input-path key (`audio_path`,
    // `file_path`, `input`, `script_path`, `video_path`,
    // `text_path`) MUST refuse paths that `classify_lexical_path`
    // resolves to `OutOfScope`. This collapses the round-1..round-4
    // bespoke `..`-guards into one gate.
    let workspace = tempfile::tempdir().expect("workspace dir");
    // Bait file outside the workspace — escape attempts would
    // otherwise resolve here.
    let outside = tempfile::tempdir().expect("outside dir");
    let bait = outside.path().join("passwd");
    std::fs::write(&bait, b"ROOT:x:0:0::/root:/bin/sh").unwrap();

    let scope = solo_scope_at(workspace.path());
    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("audio_path"),
        PathBuf::from("/bin/true"),
    );

    let outside_abs = outside.path().join("passwd").to_string_lossy().into_owned();
    for raw in [
        "../passwd",
        "../../etc/passwd",
        "foo/../../bar",
        outside_abs.as_str(),
    ] {
        let err = tool
            .rewrite_args_with_scope(&json!({"audio_path": raw}), &scope, scope.workspace())
            .expect_err(&format!("scope refuse must Err for {raw:?}"));
        let msg = err.to_string();
        assert!(
            msg.contains(raw),
            "error must echo the rejected raw path: {msg}",
        );
    }
    let _ = bait;
}

#[test]
fn plugin_refuses_out_of_scope_output_path() {
    // Phase 2-B: output-path keys (`out`, `slide_dir`) must refuse
    // `OutOfScope` paths with the same one-shot
    // `classify_lexical_path` gate. Collapses the round-4
    // `absolutize_path_in_work_dir` Err contract on output keys
    // into the unified scope policy.
    let workspace = tempfile::tempdir().expect("workspace dir");
    let scope = solo_scope_at(workspace.path());

    let def = PluginToolDef {
        name: "phase2b_output_tool".to_string(),
        description: "fixture".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "out": {"type": "string"},
                "slide_dir": {"type": "string"}
            }
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

    for (key, raw) in [
        ("out", "../sneaky"),
        ("slide_dir", "../escape"),
        ("out", ".."),
        ("slide_dir", "subdir/../../../escape"),
    ] {
        let err = tool
            .rewrite_args_with_scope(&json!({ key: raw }), &scope, scope.workspace())
            .expect_err(&format!("output key {key:?} with {raw:?} must Err"));
        let msg = err.to_string();
        assert!(
            msg.contains(raw),
            "error for {key:?}={raw:?} must echo offending path: {msg}",
        );
    }
}

#[test]
fn plugin_reads_from_shared_zone_research_dir_when_scope_present() {
    // Phase 2-B: multi-tenant `shared_zones` (e.g. `<root>/research/`,
    // `<root>/skills/`) classify as `InSharedZone`. The plugin tool
    // must allow READ from those zones with explicit intent —
    // input-path keys carry read intent by construction. Mirrors
    // the `PathClassification::InSharedZone` doc contract.
    let data = tempfile::tempdir().expect("data dir");
    let research = data.path().join("research");
    std::fs::create_dir_all(&research).expect("create research zone");
    let report = research.join("dossier.md");
    std::fs::write(&report, b"# shared dossier").unwrap();

    let scope = multi_tenant_scope_at(
        data.path(),
        "dspfac",
        "web-read-shared",
        vec![research.clone()],
    );

    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("input"),
        PathBuf::from("/bin/true"),
    );

    let rewritten = tool
        .rewrite_args_with_scope(
            &json!({"input": report.to_string_lossy().to_string()}),
            &scope,
            scope.workspace(),
        )
        .expect("read from shared zone must succeed");
    assert_eq!(
        rewritten["input"].as_str().unwrap(),
        report.to_string_lossy().to_string(),
        "shared-zone read must pass through as absolute path"
    );
}

#[test]
fn plugin_refuses_write_to_shared_zone() {
    // Phase 2-B: `InSharedZone` doc contract says reads allowed,
    // writes refused. Output-path keys (`out`, `slide_dir`) must
    // therefore Err when the path lands in a shared zone.
    let data = tempfile::tempdir().expect("data dir");
    let research = data.path().join("research");
    std::fs::create_dir_all(&research).expect("create research zone");

    let scope = multi_tenant_scope_at(
        data.path(),
        "dspfac",
        "web-write-shared",
        vec![research.clone()],
    );

    let def = PluginToolDef {
        name: "phase2b_write_shared".to_string(),
        description: "fixture".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"out": {"type": "string"}}
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

    let write_target = research.join("forbidden_output.pptx");
    let raw = write_target.to_string_lossy().to_string();
    let err = tool
        .rewrite_args_with_scope(&json!({"out": raw.clone()}), &scope, scope.workspace())
        .expect_err("writes to shared zone must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains(&raw),
        "error must echo the rejected raw path: {msg}"
    );
    assert!(
        msg.contains("shared zone") && msg.contains("read-only"),
        "error must explain shared-zone read-only policy: {msg}"
    );
}

#[test]
fn plugin_scope_path_rescues_basename_when_workspace_relative_missing() {
    // Codex P2 pin (Phase 2-B): the scope-aware rewriter must
    // preserve the legacy `resolve_path_in_work_dir` basename
    // rescue. LLMs commonly hallucinate a directory prefix in
    // front of a basename that exists at the workspace root
    // (e.g. `audio_path: "uploads/mark.wav"` when only
    // `<workspace>/mark.wav` exists from a prior attachment
    // copy). Before this fix the scope path would have rewritten
    // to the lexically-joined `<workspace>/uploads/mark.wav` and
    // the plugin's `fs::read` would fail with `os error 2`,
    // breaking attachment workflows under scoped sessions.
    let workspace = tempfile::tempdir().expect("workspace");
    let mark = workspace.path().join("mark.wav");
    std::fs::write(&mark, b"wav").unwrap();
    // `uploads/mark.wav` deliberately does NOT exist.

    let scope = solo_scope_at(workspace.path());
    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("audio_path"),
        PathBuf::from("/bin/true"),
    );

    let rewritten = tool
        .rewrite_args_with_scope(
            &json!({"audio_path": "uploads/mark.wav"}),
            &scope,
            scope.workspace(),
        )
        .expect("scope rewrite must succeed with basename rescue");
    assert_eq!(
        rewritten["audio_path"].as_str().unwrap(),
        mark.to_string_lossy().to_string(),
        "scope-aware path must rescue `<workspace>/<basename>` when the lexically-joined path is missing"
    );
}

#[test]
fn scope_still_validates_out_of_scope_when_self_work_dir_is_rebound() {
    // Codex round-2 P1 pin (Phase 2-B): the round-1 fix routed
    // hinted/rebound sessions through the legacy rewriter, which
    // only blocked `..`. The intended invariant is that
    // `SessionScope` validation applies to ALL scoped sessions,
    // EVEN when the registry rebound `self.work_dir`. Only the
    // join base for relative paths shifts; absolute or workspace-
    // relative paths still get scope-checked.
    //
    // Concretely: a hinted session with scope X and rebound
    // work_dir Y must still refuse an absolute path that escapes
    // the scope (`/etc/passwd`), even though Y is honoured for
    // CWD.
    let scope_workspace = tempfile::tempdir().expect("scope workspace");
    let rebound_work_dir = tempfile::tempdir().expect("rebound work_dir");
    // Bait file deliberately outside both dirs.
    let bait_outside = tempfile::tempdir().expect("bait");
    let bait_path = bait_outside.path().join("escape.txt");
    std::fs::write(&bait_path, b"BAIT").unwrap();

    let scope = solo_scope_at(scope_workspace.path());
    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("audio_path"),
        PathBuf::from("/bin/true"),
    );

    // Even with the rebound dir as the join base, an absolute
    // path outside the scope must still Err.
    let bait_abs = bait_path.to_string_lossy().to_string();
    let err = tool
        .rewrite_args_with_scope(
            &json!({"audio_path": bait_abs.clone()}),
            &scope,
            rebound_work_dir.path(),
        )
        .expect_err("absolute out-of-scope path must still Err under hinted wiring");
    assert!(
        err.to_string().contains(&bait_abs),
        "error must echo the rejected path: {err}",
    );

    // Defence in depth: shared-zone write refusal must also still
    // apply when the rebound work_dir would otherwise mask the
    // scope. We use a multi-tenant scope here so a shared zone
    // exists; the rebound dir is unrelated to either.
    let data = tempfile::tempdir().expect("data");
    let research = data.path().join("research");
    std::fs::create_dir_all(&research).unwrap();
    let multi_scope = multi_tenant_scope_at(
        data.path(),
        "dspfac",
        "web-codex-r2",
        vec![research.clone()],
    );
    let target_in_shared = research.join("forbidden.txt").to_string_lossy().to_string();
    let def = PluginToolDef {
        name: "phase2b_r2_output".to_string(),
        description: "fixture".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"out": {"type": "string"}}
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool_out = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));
    let err = tool_out
        .rewrite_args_with_scope(
            &json!({"out": target_in_shared.clone()}),
            &multi_scope,
            rebound_work_dir.path(),
        )
        .expect_err("shared-zone write must Err even under hinted wiring");
    assert!(
        err.to_string().contains(&target_in_shared) && err.to_string().contains("shared zone"),
        "shared-zone write error must echo the path and explain: {err}",
    );
}

#[test]
fn scope_basename_rescue_does_not_fire_for_shared_zone_misses() {
    // Codex round-2 P2 pin (Phase 2-B): the basename rescue must
    // be bounded to `InWorkspace`. A missing `InSharedZone` path
    // whose basename happens to match a workspace file MUST NOT
    // silently rewrite to the workspace file — the plugin would
    // then process different input than the LLM asked for.
    let data = tempfile::tempdir().expect("data");
    let research = data.path().join("research");
    std::fs::create_dir_all(&research).unwrap();
    let scope = multi_tenant_scope_at(
        data.path(),
        "dspfac",
        "web-rescue-bound",
        vec![research.clone()],
    );
    let workspace_file = scope.workspace().join("report.md");
    std::fs::create_dir_all(scope.workspace()).unwrap();
    std::fs::write(&workspace_file, b"# workspace report").unwrap();

    // The LLM asks for `<shared>/report.md` (missing on disk).
    // The basename `report.md` matches the workspace file. The
    // round-2 fix MUST NOT promote the workspace file.
    let missing_shared = research.join("report.md").to_string_lossy().to_string();
    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("input"),
        PathBuf::from("/bin/true"),
    );
    let rewritten = tool
        .rewrite_args_with_scope(
            &json!({"input": missing_shared.clone()}),
            &scope,
            scope.workspace(),
        )
        .expect("shared-zone read should succeed (file may be missing)");
    assert_eq!(
        rewritten["input"].as_str().unwrap(),
        missing_shared,
        "rescue MUST NOT redirect a missing shared-zone path to a basename-matching workspace file"
    );
}

#[test]
fn scope_path_rescues_skill_output_prefix_under_un_hinted_rebind() {
    // Codex round-3 P2 pin (Phase 2-B): for scoped sessions whose
    // registry rebound `self.work_dir` to
    // `<scope.workspace>/skill-output` (the typical un-hinted
    // bootstrap path), the rescue must scan the rebound work_dir.
    // Inputs like `script_path: "skill-output/mofa-podcast/intro.md"`
    // with the file actually at
    // `<scope.workspace>/skill-output/mofa-podcast/intro.md` must
    // resolve correctly — the legacy `strip_redundant_skill_output_prefix`
    // logic that `resolve_plugin_input_path` performs MUST still
    // be reachable from the scope-aware path.
    let workspace = tempfile::tempdir().expect("workspace");
    let skill_output = workspace.path().join("skill-output");
    let podcast_dir = skill_output.join("mofa-podcast");
    std::fs::create_dir_all(&podcast_dir).unwrap();
    let script = podcast_dir.join("intro.md");
    std::fs::write(&script, b"# podcast").unwrap();

    let scope = solo_scope_at(workspace.path());
    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("script_path"),
        PathBuf::from("/bin/true"),
    );

    // Mimic the routing in `prepare_effective_args`: scope is
    // present AND `self.work_dir == <scope.workspace>/skill-output`
    // (inside scope), so the join_base shifts to the rebound dir.
    let rewritten = tool
        .rewrite_args_with_scope(
            &json!({"script_path": "skill-output/mofa-podcast/intro.md"}),
            &scope,
            &skill_output,
        )
        .expect("scope rewrite must succeed");
    let resolved = rewritten["script_path"].as_str().unwrap();
    let resolved_canon = std::fs::canonicalize(resolved).unwrap_or_else(|_| {
        panic!("resolved path must exist on disk, got: {resolved}");
    });
    let expected_canon = std::fs::canonicalize(&script).expect("expected exists");
    assert_eq!(
        resolved_canon, expected_canon,
        "scoped rebind must rescue the redundant `skill-output/` prefix \
             via the legacy resolver chain"
    );
}

#[test]
fn scope_path_rescues_basename_under_un_hinted_rebind() {
    // Codex round-3 P2 pin (Phase 2-B): basename rescue inside the
    // scope path must scan the rebound `self.work_dir`, not just
    // `scope.workspace()`. When the registry rebound
    // `<scope.workspace>/skill-output` and the LLM hallucinates a
    // directory prefix in front of a basename that exists at the
    // REBOUND work_dir (`audio_path: "uploads/mark.wav"` when
    // `<scope.workspace>/skill-output/mark.wav` is the actual
    // file), the rescue must promote the rebound-dir candidate.
    let workspace = tempfile::tempdir().expect("workspace");
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    let mark = skill_output.join("mark.wav");
    std::fs::write(&mark, b"wav").unwrap();
    // `uploads/mark.wav` deliberately does NOT exist.

    let scope = solo_scope_at(workspace.path());
    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("audio_path"),
        PathBuf::from("/bin/true"),
    );

    let rewritten = tool
        .rewrite_args_with_scope(
            &json!({"audio_path": "uploads/mark.wav"}),
            &scope,
            &skill_output,
        )
        .expect("scope rewrite must succeed");
    assert_eq!(
        rewritten["audio_path"].as_str().unwrap(),
        mark.to_string_lossy().to_string(),
        "basename rescue must scan the rebound work_dir, not just scope.workspace()"
    );
}

#[test]
fn legacy_workspace_root_rescue_still_works_when_no_scope() {
    // Backward compat: when NO scope is threaded, the legacy
    // `resolve_plugin_input_path` chain (including the #1189
    // workspace-root rescue for plugins chrooted to
    // `<workspace>/skill-output/`) must still rescue
    // `<workspace>/<basename>` candidates. This pin makes sure the
    // Phase 2-B migration didn't accidentally delete the legacy
    // fallback that production fleet binaries still rely on.
    let workspace = tempfile::tempdir().expect("workspace");
    let skill_output = workspace.path().join("skill-output");
    std::fs::create_dir_all(&skill_output).unwrap();
    let script = workspace.path().join("script.md");
    std::fs::write(&script, b"# script").unwrap();

    // Resolver-level rescue still kicks in.
    let resolved = resolve_plugin_input_path("script.md", &skill_output)
        .expect("workspace-root rescue must still resolve");
    assert_eq!(std::path::Path::new(&resolved), &script);

    // End-to-end via `rewrite_workspace_file_args` (the legacy
    // entry point used when `prepare_effective_args` sees no
    // scope on the ToolContext).
    let def = PluginToolDef {
        name: "podcast_generate".to_string(),
        description: "Podcast generator".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"script_path": {"type": "string"}}
        }),
        spawn_only: true,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
        .with_work_dir(skill_output.clone());
    let rewritten = tool
        .rewrite_workspace_file_args(&json!({"script_path": "script.md"}))
        .expect("legacy rewrite must succeed");
    assert_eq!(
        rewritten["script_path"].as_str().unwrap(),
        script.to_string_lossy().to_string(),
        "legacy rescue must continue to bridge workspace-root scripts"
    );
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

/// Helper: build a temp `skill_dir` with `styles/<name>.toml` entries.
fn make_skill_dir_with_styles(styles: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create skill_dir");
    let styles_dir = dir.path().join("styles");
    std::fs::create_dir_all(&styles_dir).expect("mkdir styles");
    for name in styles {
        std::fs::write(styles_dir.join(format!("{name}.toml")), b"").expect("write style");
    }
    dir
}

#[test]
fn mofa_slides_preflight_accepts_builtin_style() {
    let skill_dir = make_skill_dir_with_styles(&["nb-pro", "puer-tea", "modern-cn", "vintage-jp"]);

    let result =
        validate_mofa_slides_style(&json!({"style": "nb-pro"}), Some(skill_dir.path()), None);

    assert!(
        result.is_ok(),
        "built-in style must pass pre-flight: {result:?}"
    );
}

#[test]
fn mofa_slides_preflight_accepts_workspace_custom_style() {
    let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
    let work_dir = make_skill_dir_with_styles(&["custom-brand"]);

    let result = validate_mofa_slides_style(
        &json!({"style": "custom-brand"}),
        Some(skill_dir.path()),
        Some(work_dir.path()),
    );

    assert!(
        result.is_ok(),
        "workspace custom style must pass pre-flight: {result:?}"
    );
}

#[test]
fn mofa_slides_preflight_rejects_missing_style() {
    let skill_dir = make_skill_dir_with_styles(&["nb-pro", "puer-tea"]);
    let work_dir = make_skill_dir_with_styles(&["custom-brand"]);

    let result = validate_mofa_slides_style(
        &json!({"style": "puer-woodcut"}),
        Some(skill_dir.path()),
        Some(work_dir.path()),
    );

    let Err(msg) = result else {
        panic!("expected pre-flight to reject invalid style, got Ok");
    };
    assert!(
        msg.contains("not found"),
        "error must mention 'not found': {msg}"
    );
    assert!(
        msg.contains("Available built-in styles"),
        "error must list available built-in styles: {msg}"
    );
    // Built-in names should be present, sorted/joined.
    assert!(msg.contains("nb-pro"), "error must list nb-pro: {msg}");
    assert!(msg.contains("puer-tea"), "error must list puer-tea: {msg}");
    // Workspace custom styles listed separately.
    assert!(
        msg.contains("Available workspace custom styles"),
        "error must list workspace customs: {msg}"
    );
    assert!(
        msg.contains("custom-brand"),
        "error must list custom-brand: {msg}"
    );
    // Hint to author under work_dir/styles/.
    assert!(
        msg.contains(&format!(
            "{}/styles/puer-woodcut.toml",
            work_dir.path().display()
        )),
        "error must hint at the workspace authoring path: {msg}"
    );
}

#[test]
fn mofa_slides_preflight_passes_when_no_style_arg() {
    // No styles dir at all on disk — pre-flight must NOT touch the
    // filesystem when the LLM omits `style`. The plugin's
    // default-style fallback path is what runs in production.
    let skill_dir = tempfile::tempdir().expect("create skill_dir");
    let work_dir = tempfile::tempdir().expect("create work_dir");

    for args in [json!({}), json!({"style": ""}), json!({"style": "   "})] {
        let result =
            validate_mofa_slides_style(&args, Some(skill_dir.path()), Some(work_dir.path()));
        assert!(
            result.is_ok(),
            "missing/empty style must pass pre-flight (args={args:?}): {result:?}"
        );
    }
}

#[tokio::test]
async fn mofa_slides_preflight_only_fires_for_mofa_slides_tool() {
    // A plugin tool with a different name must NOT be gated by the
    // mofa_slides style check, even when it carries a bogus `style`
    // arg — the pre-flight is intentionally scoped to one tool.
    let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
    let executable = skill_dir.path().join("other-binary");
    std::fs::write(&executable, b"").expect("write fake exe");

    let def = PluginToolDef {
        name: "podcast_generate".to_string(),
        description: "Podcast generator".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"style": {"type": "string"}}
        }),
        spawn_only: true,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("mofa-podcast".into(), def, executable);

    let result = tool
        .pre_flight_validate(&json!({"style": "does-not-exist"}))
        .await;
    assert!(
        result.is_ok(),
        "non-mofa_slides tool must skip pre-flight even with bad style: {result:?}"
    );

    // And the mofa_slides tool with the same bogus style MUST fail.
    let mofa_def = PluginToolDef {
        name: "mofa_slides".to_string(),
        description: "Slides".to_string(),
        input_schema: json!({"type": "object", "properties": {"style": {"type": "string"}}}),
        spawn_only: true,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let mofa_executable = skill_dir.path().join("mofa-slides");
    std::fs::write(&mofa_executable, b"").expect("write fake mofa exe");
    let mofa_tool = PluginTool::new("mofa-slides".into(), mofa_def, mofa_executable);
    let mofa_result = mofa_tool
        .pre_flight_validate(&json!({"style": "does-not-exist"}))
        .await;
    assert!(
        mofa_result.is_err(),
        "mofa_slides MUST reject bad style at pre-flight: {mofa_result:?}"
    );
}

// ---- Codex review on PR #1323 regression tests ----
//
// These guard the BLOCKER + MAJOR + MINOR findings: workspace-root
// custom styles when `work_dir` is `<workspace>/skill-output`,
// path-shaped style values that the mofa rewriter would otherwise
// normalize to a missing basename, and the `.toml.toml` hint bug.

#[test]
fn mofa_slides_preflight_accepts_workspace_root_custom_style_when_work_dir_is_skill_output() {
    // SessionRuntime binds the plugin work_dir to
    // `<workspace>/skill-output` (see runtime/session.rs:222), but the
    // slides prompt tells the LLM to author custom styles at
    // workspace-root `styles/{name}.toml` (slides_default.txt:62). The
    // pre-flight must probe `work_dir.parent()/styles/` when work_dir
    // basename is `skill-output`, otherwise a valid workspace-root
    // custom is falsely rejected.
    let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
    let workspace = tempfile::tempdir().expect("create workspace");
    let workspace_styles = workspace.path().join("styles");
    std::fs::create_dir_all(&workspace_styles).expect("mkdir workspace styles");
    std::fs::write(workspace_styles.join("foo.toml"), b"").expect("write workspace style");
    let work_dir = workspace.path().join("skill-output");
    std::fs::create_dir_all(&work_dir).expect("mkdir skill-output");

    let result = validate_mofa_slides_style(
        &json!({"style": "foo"}),
        Some(skill_dir.path()),
        Some(&work_dir),
    );

    assert!(
        result.is_ok(),
        "workspace-root custom style at <ws>/styles/foo.toml must pass pre-flight \
             when work_dir=<ws>/skill-output: {result:?}"
    );
}

#[test]
fn mofa_slides_preflight_rejects_traversal_style() {
    // The mofa rewriter normalizes "../etc/passwd" to basename "passwd"
    // (see normalize_mofa_style_name + tool.rs:609). Pre-flight must
    // validate that normalized basename so the bypass doesn't surface
    // as a background `success:false` the LLM never sees.
    let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
    let work_dir = tempfile::tempdir().expect("create work_dir");

    let result = validate_mofa_slides_style(
        &json!({"style": "../etc/passwd"}),
        Some(skill_dir.path()),
        Some(work_dir.path()),
    );

    let Err(msg) = result else {
        panic!("expected pre-flight to reject traversal style, got Ok");
    };
    assert!(
        msg.contains("not found") || msg.contains("not a valid style name"),
        "error must signal rejection: {msg}"
    );
}

#[test]
fn mofa_slides_preflight_rejects_absolute_path_style() {
    // The rewriter normalizes "/tmp/missing.toml" to basename
    // "missing" before the plugin runs (tool.rs:778). Pre-flight must
    // validate that, not skip path-shaped values.
    let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
    let work_dir = tempfile::tempdir().expect("create work_dir");

    let result = validate_mofa_slides_style(
        &json!({"style": "/tmp/missing.toml"}),
        Some(skill_dir.path()),
        Some(work_dir.path()),
    );

    assert!(
        result.is_err(),
        "absolute-path style with missing basename must fail pre-flight: {result:?}"
    );
}

#[test]
fn mofa_slides_preflight_hint_does_not_double_toml_suffix() {
    // When the LLM passes `style: "foo.toml"` and the file doesn't
    // exist, the authoring hint must say `styles/foo.toml`, not
    // `styles/foo.toml.toml`. The hint formatter must use the
    // normalized stem.
    let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
    let work_dir = tempfile::tempdir().expect("create work_dir");

    let result = validate_mofa_slides_style(
        &json!({"style": "foo.toml"}),
        Some(skill_dir.path()),
        Some(work_dir.path()),
    );

    let Err(msg) = result else {
        panic!("expected pre-flight to reject missing 'foo.toml', got Ok");
    };
    assert!(
        !msg.contains("foo.toml.toml"),
        "authoring hint must not double the .toml suffix: {msg}"
    );
    assert!(
        msg.contains("styles/foo.toml"),
        "authoring hint must reference styles/foo.toml: {msg}"
    );
    assert!(
        msg.contains("SKILL.md"),
        "error must reference SKILL.md custom-style authoring: {msg}"
    );
}

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
    let def = PluginToolDef {
        name: "phase2b_output_tool".to_string(),
        description: "fixture".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"out": {"type": "string"}}
        }),
        spawn_only: false,
        env: vec![],
        risk: None,
        spawn_only_message: None,
        concurrency_class: None,
    };
    let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

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
#[test]
fn plugin_accepts_input_path_inside_real_skill_dir_under_canonical_classify() {
    // Positive baseline: when the skill_dir really contains the
    // requested file (no symlinks involved), canonical classify
    // must still accept it. Without this we'd have no proof the
    // BLOCKER 1 fix didn't tighten reads off-cliff.
    let workspace = tempfile::tempdir().expect("workspace dir");
    let skill_dir = tempfile::tempdir().expect("skill dir");
    let manifest = skill_dir.path().join("SKILL.md");
    std::fs::write(&manifest, b"# example").unwrap();
    let canonical_skill_dir = std::fs::canonicalize(skill_dir.path()).expect("canonicalize");

    let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
        .expect("build solo scope")
        .with_skill_read_zones(vec![canonical_skill_dir.clone()])
        .expect("attach skill_read_zone");

    let tool = PluginTool::new(
        "plug".into(),
        input_path_def("audio_path"),
        PathBuf::from("/bin/true"),
    );

    // Pass the canonical form on the way in so the lexical-side
    // prefix check inside `classify_canonical_path` lines up on
    // platforms (macOS) where `/var/folders/...` is itself a
    // symlink to `/private/var/folders/...`. Production callers
    // already pass canonical paths because `with_skill_read_zones`
    // is fed the canonicalised list per the round-2 BLOCKER 2 fix.
    let candidate = canonical_skill_dir
        .join("SKILL.md")
        .to_string_lossy()
        .into_owned();
    let rewritten = tool
        .rewrite_args_with_scope(
            &json!({"audio_path": candidate.clone()}),
            &scope,
            scope.workspace(),
        )
        .expect("real skill_dir path must be accepted");
    let path_in = rewritten
        .get("audio_path")
        .and_then(|v| v.as_str())
        .expect("audio_path key must round-trip");
    assert!(
        path_in.starts_with(&*canonical_skill_dir.to_string_lossy()),
        "accepted path must remain inside the canonical skill_dir: {path_in}"
    );
}
