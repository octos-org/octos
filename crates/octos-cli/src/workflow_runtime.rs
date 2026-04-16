use crate::workflows;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowKind {
    DeepResearch,
    ResearchPodcast,
}

impl WorkflowKind {
    pub fn detect_forced_background(content: &str) -> Option<Self> {
        let lower = content.to_ascii_lowercase();
        if explicitly_foreground(&lower, content) {
            return None;
        }

        let has_podcast =
            lower.contains("podcast") || content.contains("播客") || content.contains("语音播客");
        let has_research_signal = lower.contains("deep research")
            || lower.contains("research")
            || lower.contains("latest")
            || lower.contains("news")
            || content.contains("研究")
            || content.contains("深入")
            || content.contains("深度")
            || content.contains("最新")
            || content.contains("今日")
            || content.contains("热点")
            || content.contains("新闻")
            || content.contains("搜索")
            || content.contains("资料");

        if has_podcast && has_research_signal {
            return Some(Self::ResearchPodcast);
        }

        let has_deep_research = lower.contains("deep research")
            || content.contains("深度研究")
            || content.contains("深入研究")
            || content.contains("深度调查")
            || content.contains("深度搜索")
            || content.contains("深度调研");
        if has_deep_research {
            return Some(Self::DeepResearch);
        }

        None
    }

    pub fn build(self) -> WorkflowInstance {
        match self {
            Self::DeepResearch => workflows::research_report::build(),
            Self::ResearchPodcast => WorkflowInstance {
                kind: self,
                label: "Research podcast".to_string(),
                ack_message:
                    "研究和播客生成已在后台启动。完成后只会发送最终音频结果到当前会话。"
                        .to_string(),
                current_phase: WorkflowPhase::new("research"),
                allowed_tools: vec![
                    "deep_search".into(),
                    "news_fetch".into(),
                    "write_file".into(),
                    "read_file".into(),
                    "list_dir".into(),
                    "glob".into(),
                    "podcast_generate".into(),
                ],
                limits: WorkflowLimits {
                    max_search_passes: Some(6),
                    max_dialogue_lines: Some(28),
                    target_audio_minutes: Some(15),
                    max_generate_calls: Some(1),
                },
                terminal_output: WorkflowTerminalOutput {
                    deliver_final_artifact_only: true,
                    deliver_media_only: true,
                    forbid_intermediate_files: true,
                    required_artifact_kind: "audio".into(),
                },
                additional_instructions: "You are a background news podcast producer. Follow the runtime-owned workflow phases in order: research, script, generate_audio, deliver_result. Research inside this worker, write a single podcast script in the exact format `[Character - voice, emotion] text`, and then call podcast_generate exactly once after the script is ready. Use the exact clone voices `clone:yangmi` for 杨幂 and `clone:douwentao` for 窦文涛 on every dialogue line. Do not substitute preset voices like alloy, nova, or vivian. Keep the research focused: gather only enough fresh evidence to support the script, stop after roughly 4-6 search passes, and do not keep recursively expanding side topics. Target a substantive but bounded final audio runtime of about 10-15 minutes unless the user explicitly asks for a longer show. That usually means about 18-28 dialogue lines total. Do not use fm_tts, voice_synthesize, or send_file. Do not deliver intermediate reports or script files. Only the final podcast audio may be delivered to the user.".to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowPhase(String);

impl WorkflowPhase {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_search_passes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_dialogue_lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_audio_minutes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_generate_calls: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTerminalOutput {
    pub deliver_final_artifact_only: bool,
    pub deliver_media_only: bool,
    pub forbid_intermediate_files: bool,
    pub required_artifact_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowInstance {
    #[serde(rename = "workflow_kind")]
    pub kind: WorkflowKind,
    pub label: String,
    pub ack_message: String,
    pub current_phase: WorkflowPhase,
    pub allowed_tools: Vec<String>,
    pub limits: WorkflowLimits,
    pub terminal_output: WorkflowTerminalOutput,
    pub additional_instructions: String,
}

impl WorkflowInstance {
    pub fn with_phase(&self, phase: WorkflowPhase) -> Self {
        let mut next = self.clone();
        next.current_phase = phase;
        next
    }
}

fn explicitly_foreground(lower: &str, original: &str) -> bool {
    lower.contains("wait synchronously")
        || lower.contains("wait for completion")
        || lower.contains("don't use background")
        || lower.contains("do not use background")
        || original.contains("不要后台")
        || original.contains("别后台")
        || original.contains("同步")
        || original.contains("等待完成")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_deep_research() {
        assert_eq!(
            WorkflowKind::detect_forced_background(
                "请对「全球AI代理竞争格局」做一次深度研究，并输出完整报告。"
            ),
            Some(WorkflowKind::DeepResearch)
        );
    }

    #[test]
    fn detects_research_podcast() {
        assert_eq!(
            WorkflowKind::detect_forced_background(
                "用杨幂和窦文涛的声音做一个播客，播报一下北京今日的热点新闻，要求专业冷静。"
            ),
            Some(WorkflowKind::ResearchPodcast)
        );
    }

    #[test]
    fn respects_foreground_override() {
        assert_eq!(
            WorkflowKind::detect_forced_background(
                "请同步等待完成，不要后台。对这个主题做深度研究并直接在这里输出。"
            ),
            None
        );
    }

    #[test]
    fn workflow_instance_serializes_spawn_metadata_shape() {
        let workflow = WorkflowKind::DeepResearch.build();
        let value = serde_json::to_value(&workflow).unwrap();
        assert_eq!(value.get("workflow_kind").and_then(|v| v.as_str()), Some("deep_research"));
        assert_eq!(value.get("kind"), None);
        assert_eq!(value.get("current_phase").and_then(|v| v.as_str()), Some("research"));
    }
}
