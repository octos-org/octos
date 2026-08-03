use super::*;

#[test]
fn validator_kind_label_matches_spec() {
    assert_eq!(
        validator_kind_label(&ValidatorSpec::Command {
            cmd: "x".into(),
            args: Vec::new()
        }),
        "command"
    );
    assert_eq!(
        validator_kind_label(&ValidatorSpec::ToolCall {
            tool: "x".into(),
            args: serde_json::Value::Null
        }),
        "tool_call"
    );
    assert_eq!(
        validator_kind_label(&ValidatorSpec::FileExists {
            path: "x".into(),
            min_bytes: None
        }),
        "file_exists"
    );
    assert_eq!(
        validator_kind_label(&ValidatorSpec::HttpProbe {
            url_template: "http://x".into(),
            expected_status: 200,
            expected_contains: None,
        }),
        "http_probe"
    );
    assert_eq!(
        validator_kind_label(&ValidatorSpec::OminixVoiceExists {
            name_arg: "name".into()
        }),
        "ominix_voice_exists"
    );
    assert_eq!(
        validator_kind_label(&ValidatorSpec::AudioNonSilent {
            glob: "*.wav".into(),
            min_ratio: 0.3,
            source: ValidatorFileSource::Glob,
            extension: None,
        }),
        "audio_non_silent"
    );
    assert_eq!(
        validator_kind_label(&ValidatorSpec::PerFileNonSilent {
            glob: "**/seg_*.wav".into(),
            min_ratio: 0.3,
            require_at_least: 1,
            source: ValidatorFileSource::Glob,
            extension: None,
        }),
        "per_file_non_silent"
    );
    assert_eq!(
        validator_kind_label(&ValidatorSpec::MagicBytes {
            glob: "*.mp3".into(),
            format: crate::workspace_policy::MagicByteKind::Mp3,
            source: ValidatorFileSource::Glob,
            extension: None,
        }),
        "magic_bytes"
    );
}

#[test]
fn truncate_tail_preserves_tail_on_overflow() {
    let input = "a".repeat(128);
    let out = truncate_tail(&input, 16);
    assert!(out.starts_with("...[truncated]\n"));
    assert!(out.ends_with("aaaaaaaaaaaaaaaa"));
}

#[test]
fn schema_version_is_pinned() {
    assert_eq!(VALIDATOR_RESULT_SCHEMA_VERSION, 1);
}

#[test]
fn required_gate_passes_only_on_pass() {
    let mut outcome = ValidatorOutcome {
        schema_version: VALIDATOR_RESULT_SCHEMA_VERSION,
        validator_id: "x".into(),
        phase: ValidatorPhase::Completion,
        kind: "command".into(),
        repo_label: "slides/x".into(),
        required: true,
        required_tier: "hard".into(),
        status: ValidatorStatus::Pass,
        reason: String::new(),
        duration_ms: 0,
        evidence_path: None,
        stderr: None,
        started_at: Utc::now(),
    };
    assert!(outcome.required_gate_passed());
    outcome.status = ValidatorStatus::Fail;
    assert!(!outcome.required_gate_passed());
    outcome.status = ValidatorStatus::Timeout;
    assert!(!outcome.required_gate_passed());
    outcome.status = ValidatorStatus::Error;
    assert!(!outcome.required_gate_passed());

    outcome.required = false;
    outcome.required_tier = "none".into();
    outcome.status = ValidatorStatus::Fail;
    assert!(outcome.required_gate_passed());
}

// ---------------------------------------------------------------------
// Helpers for the domain-validator tests (HTTP probe, audio, magic bytes)
// ---------------------------------------------------------------------

use std::io::{Read, Write as IoWrite};
use std::net::TcpListener;

fn dummy_invocation(workspace_root: PathBuf) -> ValidatorInvocation {
    ValidatorInvocation::new(ValidatorPhase::Completion, workspace_root, "test".into())
}

fn validator_with_spec(id: &str, spec: ValidatorSpec) -> Validator {
    Validator {
        id: id.into(),
        required: true,
        soft_fail: false,
        timeout_ms: Some(2000),
        phase: ValidatorPhaseKind::Completion,
        spec,
    }
}

/// Tiny synchronous HTTP server scripted via `responses`. Spawns a thread,
/// listens on `127.0.0.1:0`, replies to each accepted connection in order,
/// and exits once `responses.len()` connections have been served. Returns
/// the listener's bound `host:port` for the test to point validators at.
fn spawn_test_http_server(responses: Vec<&'static str>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    std::thread::spawn(move || {
        for body in responses {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

#[tokio::test]
async fn http_probe_passes_on_expected_status_and_substring() {
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n{\"ok\":\"yangmi\"}";
    let addr = spawn_test_http_server(vec![response]);
    let url = format!("http://{addr}/voices/yangmi");
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_ok",
        ValidatorSpec::HttpProbe {
            url_template: url.clone(),
            expected_status: 200,
            expected_contains: Some("yangmi".into()),
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn http_probe_fails_on_404_status() {
    let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let addr = spawn_test_http_server(vec![response]);
    let url = format!("http://{addr}/missing");
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_404",
        ValidatorSpec::HttpProbe {
            url_template: url,
            expected_status: 200,
            expected_contains: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(outcomes[0].reason.contains("got status 404"));
}

#[tokio::test]
async fn http_probe_fails_when_body_missing_expected_substring() {
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nNOPE";
    let addr = spawn_test_http_server(vec![response]);
    let url = format!("http://{addr}/x");
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_no_substring",
        ValidatorSpec::HttpProbe {
            url_template: url,
            expected_status: 200,
            expected_contains: Some("yangmi".into()),
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(outcomes[0].reason.contains("did not contain 'yangmi'"));
}

#[tokio::test]
async fn http_probe_interpolates_args_into_url_template() {
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
    let addr = spawn_test_http_server(vec![response]);
    let url_template = format!("http://{addr}/voices/${{args.name}}");
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_input_args(serde_json::json!({"name": "yangmi"}));
    let validator = validator_with_spec(
        "probe_interp",
        ValidatorSpec::HttpProbe {
            url_template,
            expected_status: 200,
            expected_contains: None,
        },
    );
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    // The successful reason should reference the interpolated URL.
    assert!(
        outcomes[0].reason.contains("/voices/yangmi"),
        "missing interpolated value in: {}",
        outcomes[0].reason
    );
}

/// RAII guard that installs an ominix-api URL override in
/// [`TEST_OMINIX_URL_OVERRIDE`] and clears it on drop.
struct OminixUrlGuard;

impl OminixUrlGuard {
    fn install(url: String) -> Self {
        *test_ominix_url_override().lock().unwrap() = Some(url);
        Self
    }
}

impl Drop for OminixUrlGuard {
    fn drop(&mut self) {
        *test_ominix_url_override().lock().unwrap() = None;
    }
}

/// Serialize ominix tests on the shared URL override slot. Using an
/// async-aware `tokio::sync::Mutex` here so the guard can safely cross
/// `.await` points (the test holds it across the in-test HTTP probe).
fn ominix_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[tokio::test]
async fn ominix_voice_exists_passes_when_name_in_voice_list() {
    let _serial = ominix_test_lock().lock().await;
    let body = "{\"voices\":[{\"name\":\"vivian\",\"aliases\":[]},{\"name\":\"serena\"}]}";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let leaked: &'static str = Box::leak(response.into_boxed_str());
    let addr = spawn_test_http_server(vec![leaked]);
    let _guard = OminixUrlGuard::install(format!("http://{addr}"));
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_input_args(serde_json::json!({"name": "vivian"}));
    let validator = validator_with_spec(
        "voice_pass",
        ValidatorSpec::OminixVoiceExists {
            name_arg: "name".into(),
        },
    );
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn ominix_voice_exists_fails_with_available_list_on_missing_name() {
    let _serial = ominix_test_lock().lock().await;
    let body = "{\"voices\":[{\"name\":\"vivian\"},{\"name\":\"serena\"}]}";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let leaked: &'static str = Box::leak(response.into_boxed_str());
    let addr = spawn_test_http_server(vec![leaked]);
    let _guard = OminixUrlGuard::install(format!("http://{addr}"));
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_input_args(serde_json::json!({"name": "yangmi"}));
    let validator = validator_with_spec(
        "voice_fail",
        ValidatorSpec::OminixVoiceExists {
            name_arg: "name".into(),
        },
    );
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    // Failure message must surface the available list so the LLM can
    // react in one round.
    assert!(
        outcomes[0].reason.contains("yangmi"),
        "missing requested name in reason: {}",
        outcomes[0].reason
    );
    assert!(
        outcomes[0].reason.contains("vivian") && outcomes[0].reason.contains("serena"),
        "missing available list in reason: {}",
        outcomes[0].reason
    );
}

/// Generate a WAV file at `path` filled with silence.
fn write_silent_wav(path: &Path, samples: usize) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
    for _ in 0..samples {
        writer.write_sample(0i16).expect("write sample");
    }
    writer.finalize().expect("finalize wav");
}

/// Generate a WAV sine wave at `path`. Loud enough that every sample is
/// above [`NON_SILENT_SAMPLE_FLOOR`].
fn write_sine_wav(path: &Path, samples: usize) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
    let amplitude = i16::MAX / 2;
    for index in 0..samples {
        let phase = (index as f32) * std::f32::consts::TAU * 440.0 / 8000.0;
        let value = (phase.sin() * amplitude as f32) as i16;
        // Keep value away from zero crossings to ensure non-silent floor.
        let value = if value.abs() < 4_000 { 4_000 } else { value };
        writer.write_sample(value).expect("write sample");
    }
    writer.finalize().expect("finalize wav");
}

#[tokio::test]
async fn audio_non_silent_fails_for_silent_wav() {
    let dir = tempfile::tempdir().unwrap();
    let audio_path = dir.path().join("silent.wav");
    write_silent_wav(&audio_path, 800);
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "silent_audio",
        ValidatorSpec::AudioNonSilent {
            glob: "*.wav".into(),
            min_ratio: 0.3,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("non_silent_ratio"),
        "reason should expose ratio: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn audio_non_silent_passes_for_sine_wav() {
    let dir = tempfile::tempdir().unwrap();
    let audio_path = dir.path().join("sine.wav");
    write_sine_wav(&audio_path, 800);
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "loud_audio",
        ValidatorSpec::AudioNonSilent {
            glob: "*.wav".into(),
            min_ratio: 0.3,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn magic_bytes_passes_for_valid_mp3_id3_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("song.mp3");
    let mut bytes = b"ID3".to_vec();
    bytes.extend(std::iter::repeat_n(0u8, 128));
    std::fs::write(&path, &bytes).unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "mp3_ok",
        ValidatorSpec::MagicBytes {
            glob: "*.mp3".into(),
            format: crate::workspace_policy::MagicByteKind::Mp3,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn magic_bytes_fails_when_file_is_actually_gif() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not_mp3.mp3");
    std::fs::write(&path, b"GIF87a\0\0\0").unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "mp3_bad",
        ValidatorSpec::MagicBytes {
            glob: "*.mp3".into(),
            format: crate::workspace_policy::MagicByteKind::Mp3,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
    assert!(outcomes[0].reason.contains("does not match mp3"));
}

#[test]
fn interpolate_args_substitutes_simple_key() {
    let args = serde_json::json!({"name": "yangmi"});
    let out = interpolate_args("http://x/${args.name}", Some(&args)).unwrap();
    assert_eq!(out, "http://x/yangmi");
}

#[test]
fn interpolate_args_errors_when_key_missing() {
    let args = serde_json::json!({});
    let err = interpolate_args("http://x/${args.name}", Some(&args)).unwrap_err();
    assert!(err.contains("'name'"));
}

#[tokio::test]
async fn file_exists_passes_when_args_interpolation_points_to_real_file() {
    // Mirrors the `fm_voice_save` post-condition: a templated path like
    // `voice_profiles/${args.name}.wav` must resolve against the spawn
    // task's input args. The existing `HttpProbe` validator already
    // does this; `FileExists` follows the same pattern.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("voice_profiles")).unwrap();
    let wav = dir.path().join("voice_profiles/yangmi.wav");
    std::fs::write(&wav, vec![0u8; 64]).unwrap();

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "voice_wav_exists",
        ValidatorSpec::FileExists {
            path: "voice_profiles/${args.name}.wav".into(),
            min_bytes: Some(32),
        },
    );
    let invocation = dummy_invocation(dir.path().to_path_buf())
        .with_input_args(serde_json::json!({"name": "yangmi"}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;

    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("yangmi.wav"),
        "reason should reference the interpolated path: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn file_exists_fails_when_interpolated_path_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("voice_profiles")).unwrap();
    // No file written.
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "voice_wav_exists",
        ValidatorSpec::FileExists {
            path: "voice_profiles/${args.name}.wav".into(),
            min_bytes: None,
        },
    );
    let invocation = dummy_invocation(dir.path().to_path_buf())
        .with_input_args(serde_json::json!({"name": "missing_voice"}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;

    assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("missing_voice.wav"),
        "reason should reference the interpolated path: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn file_exists_errors_when_required_arg_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "voice_wav_exists",
        ValidatorSpec::FileExists {
            path: "voice_profiles/${args.name}.wav".into(),
            min_bytes: None,
        },
    );
    // input_args missing the `name` key — interpolation should surface a
    // typed Error outcome rather than silently dropping the reference.
    let invocation =
        dummy_invocation(dir.path().to_path_buf()).with_input_args(serde_json::json!({}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;

    assert_eq!(outcomes[0].status, ValidatorStatus::Error, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("'name'"),
        "reason should name the missing arg: {}",
        outcomes[0].reason
    );
}

#[test]
fn interpolate_args_percent_encodes_reserved_characters() {
    // An LLM-controlled value MUST NOT be able to break out of the URL
    // segment it lands in. `?`, `&`, `/`, `#` etc. must be percent-
    // encoded so the resulting URL has the literal value as a single
    // path segment, not a structural separator.
    let args = serde_json::json!({"name": "evil/../?inject=1"});
    let out = interpolate_args("http://x/${args.name}", Some(&args)).unwrap();
    // The interpolated segment should not contain raw `/`, `?`, or `=`.
    let interpolated = out.strip_prefix("http://x/").expect("prefix preserved");
    assert!(
        !interpolated.contains('/'),
        "raw `/` leaked: {interpolated}"
    );
    assert!(
        !interpolated.contains('?'),
        "raw `?` leaked: {interpolated}"
    );
    assert!(
        !interpolated.contains('='),
        "raw `=` leaked: {interpolated}"
    );
}

// -------------------------------------------------------------------
// Wave-3b: `${output.X}` template interpolation tests.
// -------------------------------------------------------------------

/// Minimal valid PNG signature + chunk (1x1 transparent) used by the
/// MagicBytes test below. Only the leading PNG signature bytes are
/// inspected by the validator, but a full chunk-set keeps the file
/// recognizable to image tools.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R',
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE,
    0x42, 0x60, 0x82,
];

#[test]
fn interpolate_template_substitutes_output_key_verbatim() {
    // Tool-emitted values (`${output.X}`) come from a trusted source
    // and represent full URLs / paths. Percent-encoding would corrupt
    // them, so the substitution must be verbatim.
    let output = serde_json::json!({"deploy_url": "https://example.com/path?ref=main"});
    let out = interpolate_template("${output.deploy_url}", None, Some(&output)).unwrap();
    assert_eq!(out, "https://example.com/path?ref=main");
}

#[test]
fn interpolate_template_errors_when_output_key_missing() {
    // Mirror the `${args.X}` semantics: a missing key surfaces as a
    // hard error so the validator can produce an `Error` outcome
    // rather than silently degrading the URL.
    let output = serde_json::json!({});
    let err = interpolate_template("${output.deploy_url}", None, Some(&output)).unwrap_err();
    assert!(err.contains("'deploy_url'"), "{err}");
    assert!(err.contains("output"), "{err}");
}

#[test]
fn interpolate_template_errors_when_tool_output_is_none() {
    let err = interpolate_template("${output.deploy_url}", None, None).unwrap_err();
    assert!(err.contains("'deploy_url'"), "{err}");
}

#[test]
fn interpolate_template_mixes_args_and_output_in_one_template() {
    // A single template can reference both sources in any order.
    let args = serde_json::json!({"name": "yangmi"});
    let output = serde_json::json!({"host": "https://api.example.com"});
    let out = interpolate_template(
        "${output.host}/voices/${args.name}/check",
        Some(&args),
        Some(&output),
    )
    .unwrap();
    assert_eq!(out, "https://api.example.com/voices/yangmi/check");
}

#[test]
fn interpolate_template_keeps_args_percent_encoding_when_output_is_present() {
    // Mixed template: args path segment is percent-encoded even
    // though the template also references a tool output. Confirms the
    // two interpolation sources remain logically distinct.
    let args = serde_json::json!({"name": "evil/../?inject=1"});
    let output = serde_json::json!({"host": "https://api.example.com"});
    let out =
        interpolate_template("${output.host}/x/${args.name}", Some(&args), Some(&output)).unwrap();
    let segment = out
        .strip_prefix("https://api.example.com/x/")
        .expect("prefix preserved");
    assert!(
        !segment.contains('/'),
        "args `/` leaked into segment: {segment}"
    );
    assert!(
        !segment.contains('?'),
        "args `?` leaked into segment: {segment}"
    );
}

#[tokio::test]
async fn file_exists_resolves_output_template() {
    // `${output.X}` works inside FileExists for tools that emit a
    // structured artifact path.
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("publish/out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let index = out_dir.join("index.html");
    std::fs::write(&index, vec![0u8; 64]).unwrap();

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "published_index",
        ValidatorSpec::FileExists {
            path: "${output.publish_dir}/index.html".into(),
            min_bytes: Some(8),
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({"publish_dir": "publish/out"}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn file_exists_errors_when_required_output_key_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "needs_output",
        ValidatorSpec::FileExists {
            path: "${output.publish_dir}/index.html".into(),
            min_bytes: None,
        },
    );
    // tool_output missing the `publish_dir` key entirely.
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Error, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("'publish_dir'"),
        "{}",
        outcomes[0].reason
    );
}

// ---------------------------------------------------------------------
// Wave-3a: HttpProbeUntil — polling HTTP probe
// ---------------------------------------------------------------------

#[tokio::test]
async fn http_probe_until_passes_on_first_successful_attempt() {
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"ok\":\"done\"}";
    let addr = spawn_test_http_server(vec![response]);
    let url = format!("http://{addr}/status");
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_until_immediate",
        ValidatorSpec::HttpProbeUntil {
            url_template: url,
            expected_status: 200,
            expected_contains: Some("done".into()),
            poll_interval_ms: 50,
            deadline_ms: 2_000,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("attempt 1"),
        "first-attempt success should be surfaced: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn http_probe_resolves_output_url_template() {
    // mofa_publish-style scenario: tool emits a fully-formed deploy_url;
    // HttpProbe probes that URL verbatim (no percent-encoding).
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n<!DOCTYPE html>";
    let addr = spawn_test_http_server(vec![response]);
    let url_template = "${output.deploy_url}".to_string();
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_deploy",
        ValidatorSpec::HttpProbe {
            url_template,
            expected_status: 200,
            expected_contains: Some("<!DOCTYPE".into()),
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({
        "deploy_url": format!("http://{addr}/site"),
    }));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn http_probe_errors_when_output_deploy_url_missing() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_deploy_missing",
        ValidatorSpec::HttpProbe {
            url_template: "${output.deploy_url}".into(),
            expected_status: 200,
            expected_contains: None,
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Error, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("'deploy_url'"),
        "{}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn http_probe_expected_contains_interpolates_args_and_output() {
    // mofa_publish-style scenario where the deployed page mentions
    // both an LLM-supplied slug (args.repo_slug) and a tool-emitted
    // commit sha (output.commit_sha). Both must interpolate in the
    // expected_contains assertion.
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 26\r\n\r\nrepo=octos-site sha=abc123";
    let addr = spawn_test_http_server(vec![response]);
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_mixed",
        ValidatorSpec::HttpProbe {
            url_template: format!("http://{addr}/"),
            expected_status: 200,
            expected_contains: Some("sha=${output.commit_sha}".into()),
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_input_args(serde_json::json!({"repo_slug": "octos-site"}))
    .with_tool_output(serde_json::json!({"commit_sha": "abc123"}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn command_args_interpolate_output_key() {
    // Command's argv can reference output values for tools that emit
    // a path (e.g. propose_patch emitting `patch_path` → `git apply
    // --check ${output.patch_path}`). Verbatim substitution so the
    // path stays usable as a real filesystem argument.
    let dir = tempfile::tempdir().unwrap();
    let path_arg = dir.path().join("deploy.txt");
    std::fs::write(&path_arg, b"x").unwrap();

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "cmd_with_output",
        ValidatorSpec::Command {
            cmd: "test".into(),
            args: vec!["-f".into(), "${output.target_path}".into()],
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({
        "target_path": path_arg.to_string_lossy().to_string(),
    }));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

/// #1607 (codex-review round 2) — Docker fails CLOSED: a Docker-mode
/// sandbox cannot safely run a command validator here. Host absolute paths
/// like `${output.target_path}` don't resolve inside the `/workspace` bind
/// mount, and running them on the host instead (the prior behaviour) would
/// bypass Docker's mount/write/network confinement — the very escape #1607
/// closes. So the validator must fail closed with a typed error, NOT run on
/// the host (which would Pass). Host-independent: a Docker sandbox is
/// constructed regardless of whether the docker binary is present. Unix-only
/// for parity with the sibling command-validator tests.
#[tokio::test]
#[cfg(unix)]
async fn docker_sandbox_fails_command_validator_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path_arg = dir.path().join("deck.pptx");
    std::fs::write(&path_arg, b"x").unwrap();

    let docker: Arc<dyn Sandbox> = Arc::from(crate::sandbox::create_sandbox(
        &crate::sandbox::SandboxConfig {
            mode: crate::sandbox::SandboxMode::Docker,
            ..crate::sandbox::SandboxConfig::default()
        },
    ));
    assert!(docker.is_docker(), "guard: backend must be Docker");
    assert!(
        !docker.is_noop(),
        "guard: Docker is a real (non-no-op) backend"
    );

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf())
        .with_sandbox(docker);
    let validator = validator_with_spec(
        "docker_host_path_cmd",
        ValidatorSpec::Command {
            cmd: "test".into(),
            args: vec!["-f".into(), "${output.target_path}".into()],
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({
        "target_path": path_arg.to_string_lossy().to_string(),
    }));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    // Fails CLOSED: not run on the host (which would Pass and bypass Docker),
    // and not wrapped in `docker run` either.
    assert_eq!(outcomes[0].status, ValidatorStatus::Error, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("Docker") || outcomes[0].reason.contains("not supported"),
        "expected a fail-closed reason, got: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn command_args_error_when_output_key_missing() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "cmd_missing_output",
        ValidatorSpec::Command {
            cmd: "true".into(),
            args: vec!["${output.missing}".into()],
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Error, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("'missing'"),
        "{}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn http_probe_until_passes_after_polling_through_pending_responses() {
    // First two responses are 503s (so the probe must retry); the third
    // returns the expected 200 + substring. The polling loop must keep
    // probing until the success arrives.
    let pending = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 7\r\n\r\npending";
    let ready = "HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\ncomplete";
    let addr = spawn_test_http_server(vec![pending, pending, ready]);
    let url = format!("http://{addr}/status");
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_until_after_retry",
        ValidatorSpec::HttpProbeUntil {
            url_template: url,
            expected_status: 200,
            expected_contains: Some("complete".into()),
            poll_interval_ms: 50,
            deadline_ms: 5_000,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("attempt 3"),
        "expected retry path before success: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn http_probe_until_caps_per_probe_timeout_by_remaining_deadline() {
    // Codex review surface: with a 100ms deadline and a 1s per-probe
    // floor, the validator must NOT consume the full 1s for the last
    // probe — it should cap by the remaining deadline so the validator
    // returns ≈ at the wall-clock deadline. We point the probe at an
    // unreachable port; without the cap, a single probe would block for
    // 1s before failing. With the cap, the validator returns Fail in
    // well under 1s.
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    // Bind + immediately drop a listener so the port is closed.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("http://{addr}/never-reachable");
    let validator = validator_with_spec(
        "probe_until_short_deadline",
        ValidatorSpec::HttpProbeUntil {
            url_template: url,
            expected_status: 200,
            expected_contains: None,
            poll_interval_ms: 50,
            deadline_ms: 100,
        },
    );
    let before = std::time::Instant::now();
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    let elapsed = before.elapsed();
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    // Without the remaining-deadline cap, a single probe would block
    // ≈1s. With the cap, the validator returns within a few hundred ms
    // of the 100ms deadline. Allow generous headroom for cold CI.
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "deadline overrun: elapsed = {elapsed:?} (deadline 100ms, per-probe floor 1000ms)"
    );
}

#[tokio::test]
async fn http_probe_until_fails_with_last_response_when_deadline_expires() {
    // Always returns a 503; the probe must exhaust the deadline and
    // surface a Fail outcome with the last response summary in the
    // message so the LLM/operator can debug in one round.
    let pending = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 7\r\n\r\npending";
    let addr = spawn_test_http_server(vec![pending; 64]);
    let url = format!("http://{addr}/status");
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "probe_until_deadline",
        ValidatorSpec::HttpProbeUntil {
            url_template: url,
            expected_status: 200,
            expected_contains: None,
            poll_interval_ms: 50,
            deadline_ms: 200,
        },
    );
    let before = std::time::Instant::now();
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    let elapsed = before.elapsed();
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
    // Reason must reference the deadline and the last server reply so
    // the failure is debuggable from the ledger alone.
    assert!(
        outcomes[0].reason.contains("200ms") && outcomes[0].reason.to_lowercase().contains("503"),
        "deadline + last response should be in reason: {}",
        outcomes[0].reason
    );
    // The validator must not wildly overshoot the deadline; allow ample
    // headroom for CI scheduling jitter on cold runners.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "deadline overrun: elapsed = {elapsed:?}",
    );
}

#[tokio::test]
async fn http_probe_until_interpolates_args_into_url_template() {
    // Same interpolation contract as HttpProbe: ${args.<key>} resolves
    // against the spawn task's input args (URL-encoded path segment).
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
    let addr = spawn_test_http_server(vec![response]);
    let url_template = format!("http://{addr}/jobs/${{args.task_id}}");
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_input_args(serde_json::json!({"task_id": "abc-123"}));
    let validator = validator_with_spec(
        "probe_until_interp",
        ValidatorSpec::HttpProbeUntil {
            url_template,
            expected_status: 200,
            expected_contains: None,
            poll_interval_ms: 50,
            deadline_ms: 2_000,
        },
    );
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("/jobs/abc-123"),
        "interpolated URL should surface in reason: {}",
        outcomes[0].reason
    );
}

// ---------------------------------------------------------------------
// Wave-3a: Sha256Match
// ---------------------------------------------------------------------

#[tokio::test]
async fn sha256_match_passes_for_explicit_hex_digest_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("payload.bin");
    let bytes = b"hello, sha256 world".to_vec();
    std::fs::write(&path, &bytes).unwrap();
    let expected = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(&bytes))
    };
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "sha_ok",
        ValidatorSpec::Sha256Match {
            glob: "payload.bin".into(),
            sha256: expected.clone(),
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains(&expected),
        "matched digest should surface in reason: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn magic_bytes_glob_interpolates_output_key() {
    // MagicBytes pinned to a tool-emitted output directory.
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("publish");
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("a.png"), PNG_1X1).unwrap();

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "magic_bytes_output",
        ValidatorSpec::MagicBytes {
            glob: "${output.dir}/*.png".into(),
            format: crate::workspace_policy::MagicByteKind::Png,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({"dir": "publish"}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn audio_non_silent_glob_interpolates_output_key() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("clips");
    std::fs::create_dir_all(&out_dir).unwrap();
    write_sine_wav(&out_dir.join("a.wav"), 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "audio_output_glob",
        ValidatorSpec::AudioNonSilent {
            glob: "${output.audio_dir}/*.wav".into(),
            min_ratio: 0.3,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_tool_output(serde_json::json!({"audio_dir": "clips"}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn sha256_match_fails_when_digest_does_not_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("payload.bin");
    std::fs::write(&path, b"actual contents").unwrap();
    // A clearly different hash — all-zero is convenient as a sentinel
    // and ensures the validator surfaces a real mismatch, not a parser
    // error.
    let expected = "0".repeat(64);
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "sha_mismatch",
        ValidatorSpec::Sha256Match {
            glob: "payload.bin".into(),
            sha256: expected,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
    // Reason must surface BOTH the actual and the expected digest so
    // operators can diagnose the mismatch from the ledger.
    assert!(
        outcomes[0].reason.contains("actual=") && outcomes[0].reason.contains("expected="),
        "mismatch reason should expose both digests: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn sha256_match_interpolates_expected_hex_from_input_args() {
    // Lifts the inline `manage_skills::download_binary` checksum onto the
    // canonical validator path: a spawn task passes its manifest's
    // `sha256` field through input args, and the validator resolves it
    // via `${args.expected_sha256}` before hashing the artifact.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("skill_main");
    let bytes = b"#!/bin/sh\nexit 0\n";
    std::fs::write(&path, bytes).unwrap();
    let expected = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    };
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let invocation = dummy_invocation(dir.path().to_path_buf())
        .with_input_args(serde_json::json!({"expected_sha256": expected.clone()}));
    let validator = validator_with_spec(
        "sha_manifest_interp",
        ValidatorSpec::Sha256Match {
            glob: "skill_main".into(),
            sha256: "${args.expected_sha256}".into(),
        },
    );
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains(&expected),
        "interpolated digest should surface in reason: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn sha256_match_interpolates_glob_against_input_args_with_path_separators() {
    // Codex review surface: `Sha256Match.glob` must accept `${args.X}`
    // where the value contains `/` separators so the contract can scope
    // the digest check to a per-invocation artifact path
    // (e.g. `${args.skill_dir}/main`). Verifies the workspace policy
    // entry for `manage_skills` is functional rather than catastrophically
    // matching every binary in the workspace.
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("skills/example_v1");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let payload = b"installed skill binary v1\n";
    std::fs::write(skill_dir.join("main"), payload).unwrap();

    // Drop an unrelated binary at a sibling path; the test must not
    // cross-contaminate with its digest, proving the glob is scoped.
    let other_dir = dir.path().join("skills/unrelated");
    std::fs::create_dir_all(&other_dir).unwrap();
    std::fs::write(other_dir.join("main"), b"different binary").unwrap();

    let expected = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(payload))
    };
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let invocation =
        dummy_invocation(dir.path().to_path_buf()).with_input_args(serde_json::json!({
            "skill_dir": "skills/example_v1",
            "expected_sha256": expected.clone(),
        }));
    let validator = validator_with_spec(
        "sha_scoped_to_skill_dir",
        ValidatorSpec::Sha256Match {
            glob: "${args.skill_dir}/main".into(),
            sha256: "${args.expected_sha256}".into(),
        },
    );
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn sha256_match_rejects_traversal_segments_in_interpolated_glob() {
    // Codex review surface: ${args.X} in a glob template must not be
    // a vector for path-traversal escape from the workspace root.
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let invocation =
        dummy_invocation(dir.path().to_path_buf()).with_input_args(serde_json::json!({
            "skill_dir": "../../etc",
            "expected_sha256": "0".repeat(64),
        }));
    let validator = validator_with_spec(
        "sha_traversal",
        ValidatorSpec::Sha256Match {
            glob: "${args.skill_dir}/main".into(),
            sha256: "${args.expected_sha256}".into(),
        },
    );
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Error, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains(".."),
        "traversal rejection should surface the offending segment: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn sha256_match_errors_when_expected_hex_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("payload.bin"), b"contents").unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "sha_malformed",
        ValidatorSpec::Sha256Match {
            glob: "payload.bin".into(),
            // 32 chars, not 64 — must surface a typed Error rather than
            // silently treating a truncated/typo hash as a hash mismatch.
            sha256: "deadbeefcafef00d".repeat(2),
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Error, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("sha256_match"),
        "error reason should mention the validator: {}",
        outcomes[0].reason
    );
}

#[tokio::test]
async fn sha256_match_fails_when_no_file_matches_glob() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "sha_missing",
        ValidatorSpec::Sha256Match {
            glob: "skill_main".into(),
            sha256: "0".repeat(64),
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(outcomes[0].reason.contains("no files matched"));
}

// ---------------------------------------------------------------------
// Wave-3a: Required::Soft / soft_fail
// ---------------------------------------------------------------------

#[tokio::test]
async fn soft_fail_validator_does_not_block_required_gate_when_failing() {
    // A failing validator with `soft_fail = true` records the failure to
    // the ledger BUT does not demote the spawn task. The
    // `required_gate_passed()` invariant on the persisted outcome must
    // hold so the workspace contract gate ignores it.
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = Validator {
        id: "sub_artifact_warn".into(),
        // Hard-required *would* block — but soft_fail flips it to a
        // warning-only outcome even though `required = true`.
        required: true,
        soft_fail: true,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "sub-artifact.md".into(),
            min_bytes: None,
        },
    };
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(
        !outcomes[0].required,
        "soft_fail must serialize as required = false so legacy replayers \
             see it as a warning, not a hard-fail"
    );
    assert_eq!(outcomes[0].required_tier, "soft");
    assert!(
        outcomes[0].required_gate_passed(),
        "soft_fail outcomes must not block the required gate"
    );
    assert!(outcomes[0].is_soft_warning());
}

#[tokio::test]
async fn soft_fail_with_required_false_persists_as_soft_warning() {
    // Codex review surface: covers the surprising case where the
    // operator writes `required = false, soft_fail = true`. The truth
    // table maps this to `Required::Soft` (warning, not pure optional),
    // and the persisted outcome must carry `required_tier = "soft"` so
    // dashboards can split it from `required = false, soft_fail = false`
    // (purely informational) outcomes.
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = Validator {
        id: "soft_optional_warn".into(),
        required: false,
        soft_fail: true,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "missing-sub-artifact.md".into(),
            min_bytes: None,
        },
    };
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(
        !outcomes[0].required,
        "soft_fail surfaces as required=false"
    );
    assert_eq!(
        outcomes[0].required_tier, "soft",
        "(required=false, soft_fail=true) must record tier=soft, not none"
    );
    assert!(outcomes[0].required_gate_passed());
    assert!(outcomes[0].is_soft_warning());
}

#[tokio::test]
async fn legacy_ledger_record_without_required_tier_normalizes_on_replay() {
    // Codex review surface: legacy outcomes (pre-Wave-3a) have no
    // `required_tier` field. `read_all` must normalize the empty
    // sentinel into a tier derived from the legacy `required` field —
    // `required = true` → "hard", `required = false` → "none" — so
    // dashboards never see a misclassified "hard" for an old optional
    // failure.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy_ledger.jsonl");
    // Two legacy records: one was hard-required (`required = true`),
    // one was purely optional (`required = false`). Neither carries
    // `required_tier`.
    let legacy_hard = r#"{"schema_version":1,"validator_id":"old_hard","phase":"completion","kind":"file_exists","repo_label":"slides/x","required":true,"status":"pass","reason":"ok","duration_ms":12,"started_at":"2026-04-01T00:00:00Z"}"#;
    let legacy_optional = r#"{"schema_version":1,"validator_id":"old_optional","phase":"completion","kind":"file_exists","repo_label":"slides/x","required":false,"status":"fail","reason":"missing","duration_ms":3,"started_at":"2026-04-01T00:00:00Z"}"#;
    std::fs::write(&path, format!("{legacy_hard}\n{legacy_optional}\n")).unwrap();
    let ledger = ValidatorLedger::open(&path).unwrap();
    let outcomes = ledger.read_all().unwrap();
    assert_eq!(outcomes.len(), 2);
    let hard = outcomes
        .iter()
        .find(|o| o.validator_id == "old_hard")
        .unwrap();
    assert_eq!(hard.required_tier, "hard");
    let optional = outcomes
        .iter()
        .find(|o| o.validator_id == "old_optional")
        .unwrap();
    assert_eq!(
        optional.required_tier, "none",
        "legacy required=false must normalize to tier=none, not the default tier=hard"
    );
}

#[tokio::test]
async fn hard_required_validator_still_blocks_gate_when_failing() {
    // Symmetry probe: with `soft_fail = false` (the default), a failing
    // required validator demotes the gate as before.
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = Validator {
        id: "primary_required".into(),
        required: true,
        soft_fail: false,
        timeout_ms: None,
        phase: ValidatorPhaseKind::Completion,
        spec: ValidatorSpec::FileExists {
            path: "primary-artifact.md".into(),
            min_bytes: None,
        },
    };
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail);
    assert!(outcomes[0].required, "hard-required must serialize as true");
    assert_eq!(outcomes[0].required_tier, "hard");
    assert!(!outcomes[0].required_gate_passed());
}

// --- PerFileNonSilent --------------------------------------------------

/// Happy path: three sine-wave segments, all loud. The validator must
/// pass and the count of matched files must be surfaced in the reason
/// so operators can confirm the glob landed on the expected segments.
#[tokio::test]
async fn per_file_non_silent_passes_when_all_segments_are_loud() {
    let dir = tempfile::tempdir().unwrap();
    let seg_dir = dir.path().join("segments");
    std::fs::create_dir_all(&seg_dir).unwrap();
    write_sine_wav(&seg_dir.join("seg_000_alice.wav"), 800);
    write_sine_wav(&seg_dir.join("seg_001_bob.wav"), 800);
    write_sine_wav(&seg_dir.join("seg_002_alice.wav"), 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "per_file_loud",
        ValidatorSpec::PerFileNonSilent {
            glob: "**/segments/seg_*.wav".into(),
            min_ratio: 0.3,
            require_at_least: 1,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("3 match"),
        "reason should surface match count: {}",
        outcomes[0].reason
    );
}

/// Adversarial path: one of three segments is silent. The validator
/// must fail AND surface the offending filename (basename) so an
/// operator/LLM can localize which segment to regenerate.
#[tokio::test]
async fn per_file_non_silent_fails_when_one_segment_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let seg_dir = dir.path().join("segments");
    std::fs::create_dir_all(&seg_dir).unwrap();
    write_sine_wav(&seg_dir.join("seg_000_alice.wav"), 800);
    // The bad apple — silent samples drag the per-file ratio to 0.0.
    write_silent_wav(&seg_dir.join("seg_001_bob.wav"), 800);
    write_sine_wav(&seg_dir.join("seg_002_alice.wav"), 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "per_file_silent_segment",
        ValidatorSpec::PerFileNonSilent {
            glob: "**/segments/seg_*.wav".into(),
            min_ratio: 0.3,
            require_at_least: 1,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
    // Must name the offending file so the failure is actionable.
    assert!(
        outcomes[0].reason.contains("seg_001_bob.wav"),
        "failure must name the silent segment: {}",
        outcomes[0].reason
    );
    // Must include BOTH the measured ratio and the threshold for
    // ledger diagnostics — this mirrors the AudioNonSilent contract.
    assert!(
        outcomes[0].reason.contains("non_silent_ratio") && outcomes[0].reason.contains("min_ratio"),
        "failure must include measured and threshold ratios: {}",
        outcomes[0].reason
    );
}

/// Zero matches with a positive `require_at_least` must fail with a
/// message that surfaces both the expected minimum and the actual
/// count. Distinguishes "tool emitted zero artifacts" from
/// "tool emitted artifacts but one was silent".
#[tokio::test]
async fn per_file_non_silent_fails_when_match_count_below_require_at_least() {
    let dir = tempfile::tempdir().unwrap();
    // No segments dir at all.
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "per_file_min_count",
        ValidatorSpec::PerFileNonSilent {
            glob: "**/segments/seg_*.wav".into(),
            min_ratio: 0.3,
            require_at_least: 1,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
    // Message must surface BOTH the expected minimum and the actual
    // count so operators see "expected >=1, found 0" verbatim.
    assert!(
        outcomes[0].reason.contains(">=1") && outcomes[0].reason.contains("found 0"),
        "match-count failure must surface expected vs actual: {}",
        outcomes[0].reason
    );
}

/// `require_at_least = 0` (the serde default) is a deliberate escape
/// hatch: a per-file gate that doesn't ALSO demand a minimum count.
/// Zero matches must still be a Pass under that policy so a spawn
/// task can declare per-file invariants on optional intermediate
/// artifacts without forcing every run to produce them.
#[tokio::test]
async fn per_file_non_silent_passes_when_require_at_least_zero_and_no_matches() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "per_file_optional",
        ValidatorSpec::PerFileNonSilent {
            glob: "**/segments/seg_*.wav".into(),
            min_ratio: 0.3,
            require_at_least: 0,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

/// `${args.X}` interpolation must resolve against the spawn task's
/// input args, with path-traversal segments rejected. The happy path
/// here probes that the interpolated glob actually matches.
#[tokio::test]
async fn per_file_non_silent_glob_interpolates_args_key() {
    let dir = tempfile::tempdir().unwrap();
    let seg_dir = dir.path().join("episode42/segments");
    std::fs::create_dir_all(&seg_dir).unwrap();
    write_sine_wav(&seg_dir.join("seg_000_host.wav"), 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "per_file_interp",
        ValidatorSpec::PerFileNonSilent {
            glob: "${args.episode_dir}/segments/seg_*.wav".into(),
            min_ratio: 0.3,
            require_at_least: 1,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let invocation = dummy_invocation(dir.path().to_path_buf())
        .with_input_args(serde_json::json!({"episode_dir": "episode42"}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

/// Path-traversal in an arg value (`..`) must be rejected with an
/// Error outcome — not silently followed. Mirrors the
/// `interpolate_args_path` contract used by Sha256Match.
#[tokio::test]
async fn per_file_non_silent_rejects_path_traversal_arg_value() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "per_file_traversal",
        ValidatorSpec::PerFileNonSilent {
            glob: "${args.episode_dir}/segments/seg_*.wav".into(),
            min_ratio: 0.3,
            require_at_least: 1,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let invocation = dummy_invocation(dir.path().to_path_buf())
        .with_input_args(serde_json::json!({"episode_dir": "../etc"}));
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Error, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains(".."),
        "error must surface the rejected segment: {}",
        outcomes[0].reason
    );
}

/// Confirms the placeholder filenames emitted by mofa-podcast
/// (`pause_after_*`, `pause_line_*`, `bgm_placeholder_line_*`) do NOT
/// fall under the `**/segments/seg_*.wav` glob, even though they
/// share the `segments/` directory. Without this exclusion the per-
/// file gate would fire on the intentionally-silent pause WAVs and
/// the podcast contract would never pass.
#[tokio::test]
async fn per_file_non_silent_glob_excludes_placeholder_filenames() {
    let dir = tempfile::tempdir().unwrap();
    let seg_dir = dir.path().join("segments");
    std::fs::create_dir_all(&seg_dir).unwrap();
    // One valid (loud) dialogue segment.
    write_sine_wav(&seg_dir.join("seg_000_host.wav"), 800);
    // Inter-speaker pause + line pause + BGM placeholder — all are
    // legitimately silent because they ARE the pauses.
    write_silent_wav(&seg_dir.join("pause_after_000.wav"), 800);
    write_silent_wav(&seg_dir.join("pause_line_001.wav"), 800);
    write_silent_wav(&seg_dir.join("bgm_placeholder_line_002.wav"), 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "per_file_segment_pattern",
        ValidatorSpec::PerFileNonSilent {
            glob: "**/segments/seg_*.wav".into(),
            min_ratio: 0.3,
            require_at_least: 1,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    let outcomes = runner
        .run_all(&dummy_invocation(dir.path().to_path_buf()), &[validator])
        .await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("1 match"),
        "glob must match exactly 1 dialogue segment, not the 4 files on disk: {}",
        outcomes[0].reason
    );
}

// --- octos #1034: source = "spawn_only_files" --------------------------
//
// The validator family below covers the file-list-driven source that
// replaces the legacy glob when a contract opts in via
// `source = "spawn_only_files"`. The plugin protocol's `files_to_send`
// (already captured by `enforce_spawn_task_contract` and threaded into
// `ValidatorInvocation::spawn_only_files`) is the authoritative path
// set the skill produced — globbing the workspace can no longer race
// the topic-suffixed output directory naming.

/// MagicBytes + AudioNonSilent must accept a `files_to_send` list and
/// run their checks against each file directly, bypassing the glob.
/// This is the canonical happy-path for the octos #1034 refactor.
#[tokio::test]
async fn magic_bytes_uses_spawn_only_files_when_source_opted_in() {
    let dir = tempfile::tempdir().unwrap();
    // Topic-suffixed directory — the exact failure mode from the
    // mini5 trace (chat topic 《逐玉》 → `mofa-podcast-zhuyu/`). A glob
    // anchored at `skill-output/mofa-podcast/` would never reach this.
    let podcast_dir = dir.path().join("skill-output/mofa-podcast-zhuyu");
    std::fs::create_dir_all(&podcast_dir).unwrap();
    let mp3_path = podcast_dir.join("podcast_full_1779067937.mp3");
    let mut bytes = b"ID3".to_vec();
    bytes.extend(std::iter::repeat_n(0u8, 128));
    std::fs::write(&mp3_path, &bytes).unwrap();

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "magic_bytes_spawn_only",
        ValidatorSpec::MagicBytes {
            // Glob is intentionally something that would NOT match
            // anything on disk — proving the validator does not fall
            // back to the glob path.
            glob: String::new(),
            format: crate::workspace_policy::MagicByteKind::Mp3,
            source: ValidatorFileSource::SpawnOnlyFiles,
            extension: Some("mp3".into()),
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_spawn_only_files(vec![mp3_path]);
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

#[tokio::test]
async fn audio_non_silent_uses_spawn_only_files_when_source_opted_in() {
    let dir = tempfile::tempdir().unwrap();
    let podcast_dir = dir.path().join("skill-output/mofa-podcast-zhuyu");
    std::fs::create_dir_all(&podcast_dir).unwrap();
    // WAV here so the audio decoder path runs without the optional
    // `audio_mp3` feature gate.
    let wav_path = podcast_dir.join("podcast_full_1779067937.wav");
    write_sine_wav(&wav_path, 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "audio_non_silent_spawn_only",
        ValidatorSpec::AudioNonSilent {
            glob: String::new(),
            min_ratio: 0.3,
            source: ValidatorFileSource::SpawnOnlyFiles,
            extension: Some("wav".into()),
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_spawn_only_files(vec![wav_path]);
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

/// PerFileNonSilent also accepts the file list verbatim (for callers
/// whose intermediate artifacts ARE in `files_to_send`).
#[tokio::test]
async fn per_file_non_silent_uses_spawn_only_files_when_source_opted_in() {
    let dir = tempfile::tempdir().unwrap();
    let podcast_dir = dir.path().join("skill-output/mofa-podcast-zhuyu/segments");
    std::fs::create_dir_all(&podcast_dir).unwrap();
    let a = podcast_dir.join("seg_000.wav");
    let b = podcast_dir.join("seg_001.wav");
    write_sine_wav(&a, 800);
    write_sine_wav(&b, 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "per_file_spawn_only",
        ValidatorSpec::PerFileNonSilent {
            glob: String::new(),
            min_ratio: 0.3,
            require_at_least: 2,
            source: ValidatorFileSource::SpawnOnlyFiles,
            extension: Some("wav".into()),
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_spawn_only_files(vec![a, b]);
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

/// The extension filter must distinguish files by suffix so a contract
/// can pin "the mp3" without false-matching adjacent WAV intermediates
/// that may also live in `files_to_send`.
#[tokio::test]
async fn spawn_only_files_extension_filter_distinguishes_mp3_from_wav() {
    let dir = tempfile::tempdir().unwrap();
    let topic_dir = dir.path().join("skill-output/mofa-podcast-zhuyu");
    std::fs::create_dir_all(&topic_dir).unwrap();
    // Two files emitted by the plugin: an MP3 (the real deliverable)
    // and a WAV intermediate the plugin also chose to expose.
    let mp3 = topic_dir.join("podcast_full.mp3");
    let mut bytes = b"ID3".to_vec();
    bytes.extend(std::iter::repeat_n(0u8, 128));
    std::fs::write(&mp3, &bytes).unwrap();
    let wav = topic_dir.join("podcast_full.wav");
    // Write a NON-mp3 byte pattern (RIFF/WAVE-shaped) so the magic
    // check on the wav file would fail if it leaked through the
    // filter — proving the extension filter actually gates the input.
    std::fs::write(&wav, b"RIFF\0\0\0\0WAVE").unwrap();

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "magic_bytes_mp3_only",
        ValidatorSpec::MagicBytes {
            glob: String::new(),
            format: crate::workspace_policy::MagicByteKind::Mp3,
            source: ValidatorFileSource::SpawnOnlyFiles,
            extension: Some("mp3".into()),
        },
    );
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    // Both files are reported by the plugin — the filter selects the mp3.
    .with_spawn_only_files(vec![mp3, wav]);
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

/// Regression guard: existing contracts that did NOT opt in keep the
/// glob path verbatim. The `spawn_only_files` invocation attached to
/// the call must not be consulted when `source = "glob"` (the default).
#[tokio::test]
async fn glob_source_still_used_when_not_opted_in_even_with_files_to_send_present() {
    let dir = tempfile::tempdir().unwrap();
    let real_path = dir.path().join("a.png");
    std::fs::write(&real_path, PNG_1X1).unwrap();

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "magic_bytes_legacy_glob",
        ValidatorSpec::MagicBytes {
            glob: "*.png".into(),
            format: crate::workspace_policy::MagicByteKind::Png,
            source: ValidatorFileSource::Glob,
            extension: None,
        },
    );
    // Attach a `files_to_send` list pointing at a DIFFERENT path that
    // does not exist on disk. The validator must ignore the list and
    // still satisfy via the glob match.
    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "test".into(),
    )
    .with_spawn_only_files(vec![dir.path().join("nonexistent.png")]);
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Pass, "{outcomes:?}");
}

/// A contract that opts into `spawn_only_files` from a context where
/// the spawn_only tool emitted no files must Fail (not Pass via empty
/// match) so a misconfigured policy surfaces early.
#[tokio::test]
async fn spawn_only_files_source_fails_when_files_to_send_list_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let validator = validator_with_spec(
        "magic_bytes_no_files",
        ValidatorSpec::MagicBytes {
            glob: String::new(),
            format: crate::workspace_policy::MagicByteKind::Mp3,
            source: ValidatorFileSource::SpawnOnlyFiles,
            extension: Some("mp3".into()),
        },
    );
    let invocation = dummy_invocation(dir.path().to_path_buf());
    let outcomes = runner.run_all(&invocation, &[validator]).await;
    assert_eq!(outcomes[0].status, ValidatorStatus::Fail, "{outcomes:?}");
    assert!(
        outcomes[0].reason.contains("spawn_only_files"),
        "reason should surface the source: {}",
        outcomes[0].reason
    );
}

// --- octos #1036: mofa_slides sweep -----------------------------------
//
// Mirror the octos #1034 podcast happy-path test for the mofa_slides
// contract that PR #1035 left on the glob path. Failure mode being
// closed: a recursive `**/*.pptx` glob would match unrelated stale
// decks from earlier runs in the same session workspace.
//
// The `voice_synthesize` contract was originally part of this sweep
// but was dropped after codex review caught that the voice plugin's
// `succeed()` path emits only `{output, success}` (no `files_to_send`)
// and its success text `"Generated audio: <path>"` is not one of the
// prefixes `PluginTool::detect_output_file` recognises. The marker
// was fixed in PR #1039 and the voice_synthesize sweep follows below.

/// `mofa_slides` MagicBytes(Pptx) must run against the plugin's reported
/// PPTX path verbatim, including outputs at arbitrary depth where the
/// session workspace may contain unrelated PPTXs from earlier runs.
#[tokio::test]
async fn mofa_slides_uses_spawn_only_files_at_arbitrary_depth() {
    let dir = tempfile::tempdir().unwrap();
    // Deeply-nested project-style path (`<project>/output/deck.pptx`).
    // Lay down a SECOND stale PPTX elsewhere in the workspace — the
    // legacy `**/*.pptx` glob would match either, but the
    // spawn_only_files path must inspect only the reported file.
    let project_out = dir.path().join("slides/demo/output");
    std::fs::create_dir_all(&project_out).unwrap();
    let pptx_path = project_out.join("deck.pptx");
    let mut pptx_bytes = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_bytes.extend(std::iter::repeat_n(0u8, 256));
    std::fs::write(&pptx_path, &pptx_bytes).unwrap();
    // Stale, structurally-broken PPTX from a prior run. If the
    // validator ever fell back to the glob path it would pick this
    // up first (alphabetical glob order under `**`) and fail —
    // satisfying the test only via the spawn_only_files path.
    let stale = dir.path().join("aaa-stale.pptx");
    std::fs::write(&stale, b"<!DOCTYPE html>\n<html>old error</html>\n").unwrap();

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let session_policy = crate::workspace_policy::WorkspacePolicy::for_session();
    let contract = session_policy
        .spawn_tasks
        .get("mofa_slides")
        .expect("mofa_slides contract must be registered");
    let validators: Vec<Validator> = contract
        .on_completion
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            crate::workspace_policy::SpawnTaskValidatorSpec::into_validator(
                entry.clone(),
                "mofa_slides",
                i,
            )
        })
        .collect();

    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "mofa_slides".into(),
    )
    .with_spawn_only_files(vec![pptx_path]);
    let outcomes = runner.run_all(&invocation, &validators).await;
    assert!(
        outcomes.iter().all(|o| o.status == ValidatorStatus::Pass),
        "mofa_slides contract must satisfy via spawn_only_files even when an \
             unrelated stale PPTX exists in the workspace; outcomes = {outcomes:?}",
    );
}

// --- octos #1038: voice_synthesize sweep ------------------------------
//
// Mirror the octos #1036 mofa_slides happy-path test for the voice
// contract that PR #1037's revert (772783e7) left on the glob path.
// Failure mode being closed: a recursive
// `skill-output/voice/**/*.{mp3,wav}` glob would match unrelated
// stale audio from earlier runs in the same session workspace, the
// same fragility we already closed for `podcast_generate` (#1034)
// and `mofa_slides` (#1036). Predecessor PR #1039 fixed the voice
// plugin's success-line marker so `files_to_send` is now populated
// and the sweep is finally safe.

/// `voice_synthesize` AudioNonSilent must run against the plugin's
/// reported audio path verbatim. The reported file lives OUTSIDE
/// the legacy `skill-output/voice/**/*.{mp3,wav}` glob root, so a
/// validator that fell back to the glob would never see it — the
/// test would only pass via the spawn_only_files code path.
/// (The failure-mode counterpart below plants a non-silent file
/// INSIDE the legacy glob root to prove the validator does not
/// consult that path at all.)
#[tokio::test]
async fn voice_synthesize_uses_spawn_only_files_with_audio_outside_legacy_glob_root() {
    let dir = tempfile::tempdir().unwrap();
    // Real, non-silent output the plugin would report via the
    // `Generated: <path>` marker that PR #1039 introduced. The
    // plugin writes to whatever path the LLM gave it, which is
    // often OUTSIDE the legacy `skill-output/voice/` root — e.g.
    // a project-scoped subdirectory or a tempdir-style path. This
    // test pins down that the validator picks up the reported file
    // regardless of where it lives in the workspace.
    let project_dir = dir.path().join("projects/demo/audio");
    std::fs::create_dir_all(&project_dir).unwrap();
    let fresh = project_dir.join("synthesized_1779067937.wav");
    write_sine_wav(&fresh, 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let session_policy = crate::workspace_policy::WorkspacePolicy::for_session();
    let contract = session_policy
        .spawn_tasks
        .get("voice_synthesize")
        .expect("voice_synthesize contract must be registered");
    let validators: Vec<Validator> = contract
        .on_completion
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            crate::workspace_policy::SpawnTaskValidatorSpec::into_validator(
                entry.clone(),
                "voice_synthesize",
                i,
            )
        })
        .collect();

    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "voice_synthesize".into(),
    )
    .with_spawn_only_files(vec![fresh]);
    let outcomes = runner.run_all(&invocation, &validators).await;
    assert!(
        outcomes.iter().all(|o| o.status == ValidatorStatus::Pass),
        "voice_synthesize contract must satisfy via spawn_only_files for a file \
             outside the legacy `skill-output/voice/**/*` glob root (a glob fallback \
             would never have matched this path); outcomes = {outcomes:?}",
    );
}

/// Belt-and-suspenders: when the plugin reports ONLY a silent .wav
/// (the failure mode the contract exists to catch), the validator
/// must surface a Fail outcome even when an unrelated non-silent
/// .wav lives in the workspace. The legacy glob would have matched
/// the unrelated file and silently satisfied the contract.
#[tokio::test]
async fn voice_synthesize_spawn_only_files_fails_when_reported_audio_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let voice_dir = dir.path().join("skill-output/voice");
    std::fs::create_dir_all(&voice_dir).unwrap();
    // The plugin reported a silent file — this is the failure case.
    let silent = voice_dir.join("synthesized_1779067937.wav");
    write_silent_wav(&silent, 800);
    // A NON-silent file from an earlier run; if the validator ever
    // fell back to the glob it would pick this up and pass, masking
    // the failure. The spawn_only_files path must ignore it.
    let unrelated = voice_dir.join("aaa-fresh.wav");
    write_sine_wav(&unrelated, 800);

    let runner = ValidatorRunner::new(Arc::new(ToolRegistry::new()), dir.path().to_path_buf());
    let session_policy = crate::workspace_policy::WorkspacePolicy::for_session();
    let contract = session_policy
        .spawn_tasks
        .get("voice_synthesize")
        .expect("voice_synthesize contract must be registered");
    let validators: Vec<Validator> = contract
        .on_completion
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            crate::workspace_policy::SpawnTaskValidatorSpec::into_validator(
                entry.clone(),
                "voice_synthesize",
                i,
            )
        })
        .collect();

    let invocation = ValidatorInvocation::new(
        ValidatorPhase::Completion,
        dir.path().to_path_buf(),
        "voice_synthesize".into(),
    )
    .with_spawn_only_files(vec![silent]);
    let outcomes = runner.run_all(&invocation, &validators).await;
    assert!(
        outcomes.iter().any(|o| o.status == ValidatorStatus::Fail),
        "voice_synthesize contract must FAIL on a silent reported file even when \
             an unrelated non-silent file exists in the workspace (proves the validator \
             does not fall back to the glob path); outcomes = {outcomes:?}",
    );
}

#[tokio::test]
async fn map_tool_dispatcher_snapshots_all_tools_without_a_provider_policy() {
    // #1607 (P2): with no provider policy, `from_registry` snapshots every
    // registered tool, so a ToolCall validator can dispatch it.
    let reg = ToolRegistry::with_builtins(std::env::temp_dir());
    let dispatcher = MapToolDispatcher::from_registry(&reg);
    // `read_file` is a built-in and must be dispatchable.
    assert!(dispatcher.tools.contains_key("read_file"));
}

#[tokio::test]
async fn map_tool_dispatcher_excludes_provider_policy_denied_tools() {
    // #1607 (P2): a tool the provider policy denies must NOT be snapshotted
    // by `from_registry`, so a nested-project ToolCall validator can't
    // reach it. `dispatch` then reports it as unregistered instead of
    // silently invoking `Tool::execute` (which bypasses the policy gate).
    let mut reg = ToolRegistry::with_builtins(std::env::temp_dir());
    reg.set_provider_policy(crate::tools::ToolPolicy {
        deny: vec!["shell".to_string()],
        ..Default::default()
    });
    let dispatcher = MapToolDispatcher::from_registry(&reg);
    assert!(
        !dispatcher.tools.contains_key("shell"),
        "provider-policy-denied tools must be excluded from the validator dispatch snapshot"
    );
    // Dispatching the denied tool fails closed with the unregistered error.
    let err = match dispatcher
        .dispatch("shell", &serde_json::json!({ "command": "echo hi" }))
        .await
    {
        Ok(_) => panic!("denied tool must not dispatch through the snapshot"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("not registered for validator dispatch"),
        "unexpected error: {err}"
    );
    // A non-denied built-in is still present.
    assert!(dispatcher.tools.contains_key("read_file"));
}

#[cfg(unix)]
#[test]
fn kill_child_process_uses_absolute_paths() {
    // HIGH (controller-hijack): `kill_child_process` runs on a git-op TIMEOUT as
    // the CONTROLLER (unsandboxed). If it invoked `kill`/`ps` by BARE name a
    // full-FS worker could plant a fake `kill`/`ps` earlier in `$PATH` (a
    // daemon-writable dir) and get the controller to run it. Both must resolve to
    // an ABSOLUTE path so `$PATH` is never consulted.
    assert!(
        KILL_BIN.is_absolute(),
        "the timeout-kill must invoke `kill` by ABSOLUTE path (no $PATH lookup), got {:?}",
        *KILL_BIN
    );
    assert!(
        PS_BIN.is_absolute(),
        "process enumeration must invoke `ps` by ABSOLUTE path (no $PATH lookup), got {:?}",
        *PS_BIN
    );

    // The resolver never falls back to a bare (PATH-looked-up) name: even when no
    // candidate exists, the fallback is itself absolute.
    let missed = resolve_system_binary(&["/no/such/dir/kill"], "/bin/kill");
    assert_eq!(missed, PathBuf::from("/bin/kill"));
    assert!(missed.is_absolute());

    // A candidate that DOES exist is preferred over the fallback (and is absolute).
    let hit = resolve_system_binary(&["/nope/ps", "/bin/sh"], "/unused/fallback");
    assert_eq!(hit, PathBuf::from("/bin/sh"));
    assert!(hit.is_absolute());
}
