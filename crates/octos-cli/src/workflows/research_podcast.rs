use crate::workflow_runtime::{
    WorkflowInstance, WorkflowKind, WorkflowLimits, WorkflowPhase, WorkflowTerminalOutput,
};

pub fn build() -> WorkflowInstance {
    WorkflowInstance {
        kind: WorkflowKind::ResearchPodcast,
        label: "Research podcast".to_string(),
        ack_message: "研究和播客生成已在后台启动。完成后只会发送最终音频结果到当前会话。"
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
            ..Default::default()
        },
        terminal_output: WorkflowTerminalOutput {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "audio".into(),
        },
        additional_instructions: "You are a background news podcast producer. Follow the runtime-owned workflow phases in order: research, write_script, generate_audio, deliver_result. Research inside this worker, write a single podcast script in the exact format `[Character - voice, emotion] text`, and then call podcast_generate exactly once after the script is ready. Use the exact clone voices `clone:yangmi` for 杨幂 and `clone:douwentao` for 窦文涛 on every dialogue line. Do not substitute preset voices like alloy, nova, or vivian. Keep the research focused: gather only enough fresh evidence to support the script, stop after roughly 4-6 search passes, and do not keep recursively expanding side topics. Target a substantive but bounded final audio runtime of about 10-15 minutes unless the user explicitly asks for a longer show. That usually means about 18-28 dialogue lines total. Do not use fm_tts, voice_synthesize, or send_file. Do not deliver intermediate reports or script files. Only the final podcast audio may be delivered to the user.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_podcast_workflow_uses_audio_only_terminal_output() {
        let workflow = build();
        assert_eq!(workflow.kind, WorkflowKind::ResearchPodcast);
        assert_eq!(workflow.current_phase.as_str(), "research");
        assert_eq!(workflow.terminal_output.required_artifact_kind, "audio");
        assert!(workflow.terminal_output.deliver_final_artifact_only);
        assert!(workflow.terminal_output.forbid_intermediate_files);
    }
}
