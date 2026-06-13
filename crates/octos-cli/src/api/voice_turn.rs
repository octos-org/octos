//! 语音轮（voice turn）STT/TTS 封装。
//!
//! 把 serve/WS turn 路径需要的两件事——"音频→文本"与"文本→音频文件"——
//! 收敛成两个无状态 async 函数，包住共享的 `OminixClient`。turn 状态机
//! （见 `ui_protocol.rs`）只调用这里，不直接碰 ominix。

use std::path::{Path, PathBuf};

use octos_llm::ominix::OminixClient;

/// 解析 OminiX 服务基址（平台级，env 优先）。与 `api/admin.rs` 的同名 helper 等价；
/// 抽到此处避免跨模块可见性问题。
// TODO(later-tasks): remove dead_code allow once callers are wired up.
#[allow(dead_code)]
fn ominix_base_url() -> String {
    const DEFAULT: &str = "http://localhost:8080";
    std::env::var("OMINIX_API_URL").unwrap_or_else(|_| DEFAULT.to_string())
}

/// 从混合媒体路径里挑出音频文件，保持原顺序。
// TODO(later-tasks): remove dead_code allow once callers are wired up.
#[allow(dead_code)]
pub(crate) fn audio_paths(media: &[String]) -> Vec<String> {
    media
        .iter()
        .filter(|p| octos_bus::media::is_audio(p))
        .cloned()
        .collect()
}

/// 转写 turn 内全部音频媒体。无音频时返回空 vec（调用方据此判定是否"语音轮"）。
/// 单条转写失败只记日志并跳过，不让整轮失败。
// TODO(later-tasks): remove dead_code allow once callers are wired up.
#[allow(dead_code)]
pub(crate) async fn transcribe_audio_media(
    media: &[String],
    language: Option<&str>,
) -> Vec<String> {
    let audios = audio_paths(media);
    if audios.is_empty() {
        return Vec::new();
    }
    let client =
        OminixClient::new(&ominix_base_url()).with_language(language.map(|s| s.to_string()));
    let asr_t = std::time::Instant::now();
    let mut out = Vec::new();
    for path in audios {
        match client.transcribe(Path::new(&path)).await {
            Ok(text) if !text.trim().is_empty() => out.push(text),
            Ok(_) => tracing::warn!(audio = %path, "voice_turn: empty transcript, skipping"),
            Err(e) => tracing::warn!(audio = %path, error = %e, "voice_turn: transcription failed"),
        }
    }
    eprintln!(
        "[TIMING] ASR_done dur_ms={} epoch_ms={}",
        asr_t.elapsed().as_millis(),
        now_ms()
    );
    out
}

/// Whether a char is safe to hand to TTS: letters/digits (incl. CJK),
/// whitespace, and common sentence punctuation. Everything else — emoji,
/// pictographs, math/misc symbols — is dropped, because some on-device engines
/// (GPT-SoVITS) error out on unspeakable input instead of ignoring it.
/// Wall-clock epoch milliseconds (for cross-stage timing in voice turns).
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn is_tts_safe(c: char) -> bool {
    c.is_alphanumeric()
        || c.is_whitespace()
        || matches!(
            c,
            ',' | '.'
                | '!'
                | '?'
                | ';'
                | ':'
                | '\''
                | '"'
                | '-'
                | '('
                | ')'
                | '，'
                | '。'
                | '！'
                | '？'
                | '；'
                | '：'
                | '、'
                | '…'
                | '《'
                | '》'
                | '「'
                | '」'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '%'
        )
}

/// Strip Markdown / formatting noise so the TTS speaks clean prose instead of
/// "swallowing" or mispronouncing symbols. Removes fenced + inline code, link
/// URLs (keeping the visible text), emphasis/heading/list/quote markers, emoji /
/// pictographs / stray symbols, and collapses leftover whitespace.
pub(crate) fn clean_for_tts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue; // drop code-block bodies entirely
        }
        // Strip leading list/quote/heading markers.
        let mut s = trimmed.to_string();
        while let Some(rest) = s
            .strip_prefix("# ")
            .or_else(|| s.strip_prefix("## "))
            .or_else(|| s.strip_prefix("### "))
            .or_else(|| s.strip_prefix("> "))
            .or_else(|| s.strip_prefix("- "))
            .or_else(|| s.strip_prefix("* "))
            .or_else(|| s.strip_prefix("+ "))
        {
            s = rest.to_string();
        }
        out.push_str(&s);
        out.push('\n');
    }

    // `[label](url)` -> `label`; bare emphasis/code chars dropped.
    let mut result = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' => {
                // Capture label up to `]`, then skip an optional `(...)`.
                let mut label = String::new();
                for lc in chars.by_ref() {
                    if lc == ']' {
                        break;
                    }
                    label.push(lc);
                }
                if chars.peek() == Some(&'(') {
                    for pc in chars.by_ref() {
                        if pc == ')' {
                            break;
                        }
                    }
                }
                result.push_str(&label);
            }
            '*' | '_' | '`' | '~' | '#' => {} // drop emphasis / code markers
            other if is_tts_safe(other) => result.push(other),
            // Drop emoji / pictographs / stray symbols (😄🌙×🐙 …). Some on-device
            // TTS engines (e.g. GPT-SoVITS) abort synthesis on unspeakable chars
            // rather than skipping them, which fails the whole turn.
            _ => {}
        }
    }

    // Collapse runs of blank lines / spaces.
    result
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Ensure the text ends on a *strong* terminal boundary so the TTS engine
/// fully renders the final syllable.
///
/// On-device GPT-SoVITS (and similar) clips the last character when the input
/// ends on a soft pause mark (comma / 顿号 / 分号 / 冒号) or a bare content
/// char: it reads the trailing pause as "more is coming" and stops generating
/// mid-syllable, so e.g. `"…往下跳，"` comes back as audio that cuts off just
/// as `跳` begins. The streamed-reply chunker (`drain_voice_sentences`) emits
/// comma-terminated fragments, and the reply's trailing fragment often has no
/// punctuation at all — both hit this. Normalising the tail to a full stop
/// gives a clean sentence-final boundary the engine renders in full.
///
/// Strong terminals already present (`。！？!?…`) are left untouched.
fn ensure_terminal_punctuation(text: &str) -> String {
    const STRONG: &[char] = &['。', '！', '？', '!', '?', '…', '.'];
    const SOFT: &[char] = &['，', ',', '、', '；', ';', '：', ':'];

    let mut s = text.trim_end().to_string();
    if s.is_empty() {
        return s;
    }
    match s.chars().next_back() {
        Some(c) if STRONG.contains(&c) => return s,
        _ => {}
    }
    // Drop any trailing run of soft pause marks / whitespace, then append one
    // full stop so a comma-ended (or bare) fragment gets a real boundary.
    while let Some(c) = s.chars().next_back() {
        if SOFT.contains(&c) || c.is_whitespace() {
            s.pop();
        } else {
            break;
        }
    }
    s.push('。');
    s
}

/// Volcano Engine (ByteDance) cloud-TTS config, sourced from env. Returns
/// `None` (→ fall back to on-device ominix) unless appid + token are set.
/// Moving TTS to the cloud also stops ominix from thrashing ASR↔TTS model
/// reloads under memory pressure, so on-device STT stays fast.
struct VolcanoTts {
    appid: String,
    token: String,
    cluster: String,
    voice: String,
    encoding: String,
    endpoint: String,
}

fn volcano_from_env() -> Option<VolcanoTts> {
    let appid = std::env::var("VOLC_TTS_APPID")
        .ok()
        .filter(|s| !s.is_empty())?;
    let token = std::env::var("VOLC_TTS_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(VolcanoTts {
        appid,
        token,
        cluster: std::env::var("VOLC_TTS_CLUSTER").unwrap_or_else(|_| "volcano_tts".to_string()),
        voice: std::env::var("VOLC_TTS_VOICE").unwrap_or_else(|_| "BV001_streaming".to_string()),
        encoding: std::env::var("VOLC_TTS_ENCODING").unwrap_or_else(|_| "mp3".to_string()),
        endpoint: std::env::var("VOLC_TTS_ENDPOINT")
            .unwrap_or_else(|_| "https://openspeech.bytedance.com/api/v1/tts".to_string()),
    })
}

/// Synthesize via Volcano Engine HTTP TTS (non-streaming `operation:"query"`,
/// returns base64 audio in JSON). Writes the decoded audio to `out_dir` and
/// returns the path. `None` on any failure (caller falls back to ominix).
async fn synthesize_volcano(cfg: &VolcanoTts, text: &str, out_dir: &Path) -> Option<PathBuf> {
    use base64::Engine;

    let reqid = uuid::Uuid::now_v7().to_string();
    let body = serde_json::json!({
        "app": { "appid": cfg.appid, "token": cfg.token, "cluster": cfg.cluster },
        "user": { "uid": "octos-voice" },
        "audio": { "voice_type": cfg.voice, "encoding": cfg.encoding, "speed_ratio": 1.0 },
        "request": { "reqid": reqid, "text": text, "operation": "query", "text_type": "plain" },
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&cfg.endpoint)
        // Volcano's quirky scheme: literal "Bearer;" + token (semicolon, no space).
        .header("Authorization", format!("Bearer;{}", cfg.token))
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano TTS request failed"))
        .ok()?;

    let json: serde_json::Value = resp
        .json()
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano TTS bad JSON"))
        .ok()?;

    // code 3000 == success per Volcano TTS API.
    if json.get("code").and_then(|c| c.as_i64()) != Some(3000) {
        tracing::warn!(resp = %json, "voice_turn: volcano TTS non-success code");
        return None;
    }
    let b64 = json.get("data").and_then(|d| d.as_str())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano TTS base64 decode"))
        .ok()?;
    if bytes.is_empty() {
        return None;
    }

    let ext = if cfg.encoding == "wav" { "wav" } else { "mp3" };
    let out_path = out_dir.join(format!("reply-{reqid}.{ext}"));
    tokio::fs::write(&out_path, &bytes)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano TTS write failed"))
        .ok()?;
    Some(out_path)
}

/// Synthesize a reply to an audio file, picking the TTS route from `provider`:
/// - `"auto"`: cloud Volcano when `VOLC_TTS_*` env is configured, else
///   on-device GPT-SoVITS.
/// - `"volcano"`: force cloud Volcano; fall back to on-device sovits when the
///   env is missing or the request fails.
/// - `"sovits"` / `"qwen3"`: force the named on-device engine (no cloud).
///
/// `voice` is the on-device voice preset (voices.json); the cloud route uses
/// its own `VOLC_TTS_VOICE` env instead. Returns `None` on failure.
pub(crate) async fn synthesize_reply(
    text: &str,
    voice: &str,
    provider: &str,
    out_dir: &Path,
) -> Option<PathBuf> {
    let speak = clean_for_tts(text);
    if speak.trim().is_empty() {
        return None;
    }
    // Normalise the tail to a strong terminal so engines (notably on-device
    // GPT-SoVITS) don't clip the final syllable of comma-ended / bare fragments.
    let speak = ensure_terminal_punctuation(&speak);
    let tts_t = std::time::Instant::now();
    eprintln!("[TIMING] TTS_start epoch_ms={}", now_ms());

    // Cloud route. "auto" uses cloud only when env is present; "volcano" forces
    // it (still falls back to on-device on failure). Cloud is faster (no
    // on-device model reload) and higher quality when available.
    let want_cloud = matches!(provider, "auto" | "volcano");
    if want_cloud {
        if let Some(cfg) = volcano_from_env() {
            if let Some(path) = synthesize_volcano(&cfg, &speak, out_dir).await {
                return Some(path);
            }
            tracing::warn!("voice_turn: volcano TTS failed; falling back to ominix");
        } else if provider == "volcano" {
            tracing::warn!(
                "voice_turn: tts_provider=volcano but VOLC_TTS_* env missing; \
                 falling back to on-device sovits"
            );
        }
    }

    // On-device route. Forced engine for "sovits"/"qwen3"; sovits otherwise.
    let engine = if provider == "qwen3" {
        "qwen3"
    } else {
        "sovits"
    };
    let out_path = out_dir.join(format!("reply-{}.wav", uuid::Uuid::now_v7()));
    let client = OminixClient::new(&ominix_base_url());
    match client
        .synthesize_to_file(&speak, voice, engine, None, &out_path)
        .await
    {
        Ok(_) => {
            eprintln!(
                "[TIMING] TTS_done dur_ms={} epoch_ms={}",
                tts_t.elapsed().as_millis(),
                now_ms()
            );
            Some(out_path)
        }
        Err(e) => {
            tracing::warn!(error = %e, "voice_turn: synthesis failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn synthesize_reply_returns_none_for_blank_text() {
        let dir = std::env::temp_dir();
        let got = synthesize_reply("   ", "vivian", "auto", &dir).await;
        assert!(got.is_none());
    }

    #[test]
    fn trailing_comma_becomes_full_stop_so_final_syllable_is_not_clipped() {
        // GPT-SoVITS clips the last syllable when a chunk ends on a soft comma
        // (it reads the trailing pause as "more coming" and stops generating
        // mid-syllable). Normalising to a full stop gives a clean sentence-final
        // boundary so the engine renders the whole last character.
        assert_eq!(
            ensure_terminal_punctuation("就每天背着壳爬到山顶往下跳，"),
            "就每天背着壳爬到山顶往下跳。"
        );
    }

    #[test]
    fn bare_ending_gets_terminal_punctuation() {
        // The reply's trailing fragment often has no punctuation at all; a bare
        // content char is just as prone to clipping as a trailing comma.
        assert_eq!(
            ensure_terminal_punctuation("好的我知道了"),
            "好的我知道了。"
        );
    }

    #[test]
    fn strong_terminal_is_left_unchanged() {
        assert_eq!(ensure_terminal_punctuation("你好！"), "你好！");
        assert_eq!(ensure_terminal_punctuation("真的吗？"), "真的吗？");
        assert_eq!(ensure_terminal_punctuation("等等…"), "等等…");
        assert_eq!(ensure_terminal_punctuation("Okay."), "Okay.");
    }

    #[test]
    fn trailing_soft_marks_and_whitespace_collapse_to_one_full_stop() {
        assert_eq!(ensure_terminal_punctuation("走吧， "), "走吧。");
        assert_eq!(ensure_terminal_punctuation("等一下；"), "等一下。");
    }

    #[test]
    fn clean_for_tts_strips_markdown_noise() {
        let md = "# 标题\n\n这是**重点**和 `代码` 还有 [链接](https://x.com)。\n\n```rust\nfn main() {}\n```\n\n- 一项\n- 两项";
        let got = clean_for_tts(md);
        assert!(!got.contains('#'));
        assert!(!got.contains('*'));
        assert!(!got.contains('`'));
        assert!(!got.contains("https://"));
        assert!(!got.contains("fn main")); // code block dropped
        assert!(got.contains("这是重点和 代码 还有 链接。"));
        assert!(got.contains("一项"));
    }

    #[test]
    fn clean_for_tts_blank_when_only_formatting() {
        assert_eq!(clean_for_tts("```\ncode\n```").as_str(), "");
    }

    #[test]
    fn clean_for_tts_strips_emoji_and_symbols() {
        // GPT-SoVITS aborts synthesis on emoji/symbols; they must be dropped.
        let got = clean_for_tts("晚上好呀！😄🌙 今天第 N 次×2 打招呼 😂🐙");
        for bad in ['😄', '🌙', '😂', '🐙', '×'] {
            assert!(!got.contains(bad), "should strip {bad:?}: got {got:?}");
        }
        // Speakable content + punctuation preserved.
        assert!(got.contains("晚上好呀！"));
        assert!(got.contains("今天第 N 次"));
        assert!(got.contains("打招呼"));
    }

    #[test]
    fn audio_paths_filters_non_audio() {
        let media = vec![
            "/tmp/a/photo.png".to_string(),
            "/tmp/a/note.ogg".to_string(),
            "/tmp/a/doc.pdf".to_string(),
            "/tmp/a/clip.wav".to_string(),
        ];
        let got = audio_paths(&media);
        assert_eq!(
            got,
            vec!["/tmp/a/note.ogg".to_string(), "/tmp/a/clip.wav".to_string()]
        );
    }
}
