use serde::{Deserialize, Serialize};

#[path = "workflow_families/mod.rs"]
pub mod workflow_families;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowKind {
    DeepResearch,
    ResearchPodcast,
    Slides,
    Site,
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
        workflow_families::compile_default(self)
    }

    pub fn plan(self) -> workflow_families::WorkflowPlan {
        workflow_families::WorkflowPlanRequest::default_for_kind(self).compile()
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
    pub max_pipeline_runs: Option<u32>,
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
        assert_eq!(
            value.get("workflow_kind").and_then(|v| v.as_str()),
            Some("deep_research")
        );
        assert_eq!(value.get("kind"), None);
        assert_eq!(
            value.get("current_phase").and_then(|v| v.as_str()),
            Some("research")
        );
    }

    #[test]
    fn workflow_instance_serializes_slides_metadata_shape() {
        let workflow = WorkflowKind::Slides.build();
        let value = serde_json::to_value(&workflow).unwrap();
        assert_eq!(
            value.get("workflow_kind").and_then(|v| v.as_str()),
            Some("slides")
        );
        assert_eq!(value.get("kind"), None);
        assert_eq!(
            value.get("current_phase").and_then(|v| v.as_str()),
            Some("design")
        );
        assert_eq!(
            value
                .get("terminal_output")
                .and_then(|output| output.get("required_artifact_kind"))
                .and_then(|v| v.as_str()),
            Some("presentation")
        );
    }

    #[test]
    fn workflow_instance_serializes_site_metadata_shape() {
        let workflow = WorkflowKind::Site.build();
        let value = serde_json::to_value(&workflow).unwrap();
        assert_eq!(
            value.get("workflow_kind").and_then(|v| v.as_str()),
            Some("site")
        );
        assert_eq!(value.get("kind"), None);
        assert_eq!(
            value.get("current_phase").and_then(|v| v.as_str()),
            Some("scaffold")
        );
        assert_eq!(
            value
                .get("terminal_output")
                .and_then(|output| output.get("required_artifact_kind"))
                .and_then(|v| v.as_str()),
            Some("site")
        );
    }

    #[test]
    fn workflow_kind_plan_compiles_via_registry() {
        let plan = WorkflowKind::ResearchPodcast.plan();
        assert_eq!(plan.kind(), WorkflowKind::ResearchPodcast);

        let workflow = plan.clone().into_instance();
        assert_eq!(workflow.kind, WorkflowKind::ResearchPodcast);
        assert_eq!(workflow.current_phase.as_str(), "research");
    }

    #[test]
    fn workflow_kind_build_uses_registry_defaults() {
        let kinds = [
            WorkflowKind::DeepResearch,
            WorkflowKind::ResearchPodcast,
            WorkflowKind::Slides,
            WorkflowKind::Site,
        ];

        for kind in kinds {
            let workflow = kind.build();
            assert_eq!(workflow.kind, kind);
        }
    }

    #[test]
    fn detects_english_deep_research() {
        assert_eq!(
            WorkflowKind::detect_forced_background(
                "Please run a deep research on the agentic OS landscape"
            ),
            Some(WorkflowKind::DeepResearch)
        );
    }

    #[test]
    fn detects_deep_research_across_zh_synonyms() {
        for prompt in [
            "深度研究一下这个课题",
            "深入研究这个课题",
            "深度调查这个事件",
            "深度搜索相关资料",
            "深度调研市场情况",
        ] {
            assert_eq!(
                WorkflowKind::detect_forced_background(prompt),
                Some(WorkflowKind::DeepResearch),
                "prompt: {prompt}"
            );
        }
    }

    #[test]
    fn detects_research_podcast_in_english_and_zh_synonym() {
        assert_eq!(
            WorkflowKind::detect_forced_background("make a podcast about today's AI news"),
            Some(WorkflowKind::ResearchPodcast)
        );
        assert_eq!(
            WorkflowKind::detect_forced_background("把今日热点整理成语音播客"),
            Some(WorkflowKind::ResearchPodcast)
        );
    }

    #[test]
    fn podcast_request_without_research_signal_stays_foreground() {
        assert_eq!(
            WorkflowKind::detect_forced_background("make a podcast about our chat just now"),
            None
        );
    }

    #[test]
    fn research_signal_without_depth_or_podcast_stays_foreground() {
        assert_eq!(
            WorkflowKind::detect_forced_background("research the latest AI news for me"),
            None
        );
    }

    #[test]
    fn podcast_signal_takes_precedence_over_deep_research() {
        assert_eq!(
            WorkflowKind::detect_forced_background(
                "do a deep research on the news and then record a podcast"
            ),
            Some(WorkflowKind::ResearchPodcast)
        );
    }

    #[test]
    fn detection_is_case_insensitive_for_ascii_signals() {
        assert_eq!(
            WorkflowKind::detect_forced_background("DEEP RESEARCH the space race"),
            Some(WorkflowKind::DeepResearch)
        );
        assert_eq!(
            WorkflowKind::detect_forced_background("RESEARCH the News as a PODCAST"),
            Some(WorkflowKind::ResearchPodcast)
        );
    }

    #[test]
    fn english_foreground_overrides_each_suppress_detection() {
        for phrase in [
            "wait synchronously",
            "wait for completion",
            "don't use background",
            "do not use background",
        ] {
            let prompt = format!("deep research the topic and {phrase} right here");
            assert_eq!(
                WorkflowKind::detect_forced_background(&prompt),
                None,
                "override phrase: {phrase}"
            );
        }
    }

    #[test]
    fn chinese_foreground_overrides_each_suppress_detection() {
        for phrase in ["同步", "等待完成", "不要后台", "别后台"] {
            let prompt = format!("深度研究这个课题，但请{phrase}，直接输出结果");
            assert_eq!(
                WorkflowKind::detect_forced_background(&prompt),
                None,
                "override phrase: {phrase}"
            );
        }
    }

    #[test]
    fn with_phase_replaces_only_current_phase() {
        let workflow = WorkflowKind::Site.build();
        let next = workflow.with_phase(WorkflowPhase::new("build"));
        assert_eq!(next.current_phase.as_str(), "build");
        assert_eq!(workflow.current_phase.as_str(), "scaffold");
        assert_eq!(next.kind, workflow.kind);
        assert_eq!(next.label, workflow.label);
        assert_eq!(next.allowed_tools, workflow.allowed_tools);
    }

    #[test]
    fn default_limits_serialize_to_empty_object() {
        let value = serde_json::to_value(WorkflowLimits::default()).unwrap();
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn populated_limits_skip_unset_fields() {
        let limits = WorkflowLimits {
            max_search_passes: Some(6),
            max_pipeline_runs: Some(1),
            ..WorkflowLimits::default()
        };
        let value = serde_json::to_value(&limits).unwrap();
        let map = value.as_object().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("max_search_passes").and_then(|v| v.as_u64()),
            Some(6)
        );
        assert_eq!(
            map.get("max_pipeline_runs").and_then(|v| v.as_u64()),
            Some(1)
        );
    }

    #[test]
    fn workflow_kind_serde_roundtrips_all_variants() {
        for (kind, name) in [
            (WorkflowKind::DeepResearch, "deep_research"),
            (WorkflowKind::ResearchPodcast, "research_podcast"),
            (WorkflowKind::Slides, "slides"),
            (WorkflowKind::Site, "site"),
        ] {
            let value = serde_json::to_value(kind).unwrap();
            assert_eq!(value.as_str(), Some(name), "serialize {name}");
            let parsed: WorkflowKind = serde_json::from_value(value).unwrap();
            assert_eq!(parsed, kind, "deserialize {name}");
        }
    }
}
