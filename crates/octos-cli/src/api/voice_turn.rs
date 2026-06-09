//! 语音轮（voice turn）STT/TTS 封装。
//!
//! 把 serve/WS turn 路径需要的两件事——"音频→文本"与"文本→音频文件"——
//! 收敛成两个无状态 async 函数，包住共享的 `OminixClient`。turn 状态机
//! （见 `ui_protocol.rs`）只调用这里，不直接碰 ominix。

use std::path::Path;
#[allow(unused_imports)]
use std::path::PathBuf;

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
    let client = OminixClient::new(&ominix_base_url())
        .with_language(language.map(|s| s.to_string()));
    let mut out = Vec::new();
    for path in audios {
        match client.transcribe(Path::new(&path)).await {
            Ok(text) if !text.trim().is_empty() => out.push(text),
            Ok(_) => tracing::warn!(audio = %path, "voice_turn: empty transcript, skipping"),
            Err(e) => tracing::warn!(audio = %path, error = %e, "voice_turn: transcription failed"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_paths_filters_non_audio() {
        let media = vec![
            "/tmp/a/photo.png".to_string(),
            "/tmp/a/note.ogg".to_string(),
            "/tmp/a/doc.pdf".to_string(),
            "/tmp/a/clip.wav".to_string(),
        ];
        let got = audio_paths(&media);
        assert_eq!(got, vec!["/tmp/a/note.ogg".to_string(), "/tmp/a/clip.wav".to_string()]);
    }
}
