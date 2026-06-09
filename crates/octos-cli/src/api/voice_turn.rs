//! 语音轮（voice turn）STT/TTS 封装。
//!
//! 把 serve/WS turn 路径需要的两件事——"音频→文本"与"文本→音频文件"——
//! 收敛成两个无状态 async 函数，包住共享的 `OminixClient`。turn 状态机
//! （见 `ui_protocol.rs`）只调用这里，不直接碰 ominix。

// TODO(later-tasks): remove this allow once `transcribe_audio_media` and
// `synthesize_reply` are implemented and consume these imports.
#[allow(unused_imports, dead_code)]
use std::path::{Path, PathBuf};

#[allow(unused_imports, dead_code)]
use octos_llm::ominix::OminixClient;

/// 解析 OminiX 服务基址（平台级，env 优先）。与 `api/admin.rs` 的同名 helper 等价；
/// 抽到此处避免跨模块可见性问题。
#[allow(dead_code)]
fn ominix_base_url() -> String {
    const DEFAULT: &str = "http://localhost:8080";
    std::env::var("OMINIX_API_URL").unwrap_or_else(|_| DEFAULT.to_string())
}
