spec: task
name: "压缩后当前指令优先"
tags: [context-manager, compaction, appui, prompt]
estimate: 0.5d
---

## Intent

实测(2026-08-02 16:27,kimi:local:tui#coding):turn_start 压缩(216→15 条,
94k→5k)后,模型延续了摘要里的旧任务计划,忽略了最新用户指令("先落地大纲")
——"驴唇不对马嘴"。机理:摘要提示词要求写"current goal / what remains",旧
计划被表述成当前目标;摘要行紧贴 system 位置权威度高;16 条原文保留窗口恰被
被中断回合的旧流水占满;最新用户消息只是普通一条,无任何优先级标记。本任务
在两处注入"历史仅为背景、最新用户消息才是当前指令"的显式框架:摘要生成提示
词与摘要的 prompt 渲染(头部+尾部声明),让压缩后的上下文权重结构不再撒谎。

## Decisions

- 摘要生成侧: `LLM_COMPACTION_SYSTEM_PROMPT`(octos-agent/compaction.rs)
  追加降级指令——所摘要的一切均为背景;禁止把历史目标表述为当前指令;当前
  指令永远以最新用户消息为准。仅改提示词文本,不改调用协议与超时/回退逻辑。
- 摘要渲染侧: ContextManager `for_prompt` 的 CompactionSummary 渲染,由
  `[Conversation summary]` 头改为背景框架:头部声明"以下为压缩背景",尾部
  声明"背景结束;下方最新用户消息是当前指令,优先于以上全部内容"。框架只存在
  于渲染层(prompt frame),不写入 transcript 存储,不影响 transcript hash 与
  审计链。
- 保持既有 bridge 兼容性不变: 摘要行仍为 protected 的 User 角色(User 而非
  System 的原因见该处既有注释——normalize_system_messages 会重写非首条
  System 行,破坏 bridge 覆盖窗口)。
- 未发生压缩的会话,prompt 不得出现任何背景框架文本。

## Boundaries

### Allowed Changes
- crates/octos-agent/src/compaction.rs
- **/crates/octos-agent/src/compaction.rs
- crates/octos-cli/src/api/context_manager.rs
- **/crates/octos-cli/src/api/context_manager.rs
- specs/task-compaction-instruction-priority.spec.md
- **/specs/task-compaction-instruction-priority.spec.md

### Forbidden
- 不改变压缩触发阈值、保留条数(APPUI_CONTEXT_COMPACT_KEEP_ITEMS)与
  retention 选取逻辑
- 不改变 compact_context 的存储结构、transcript hash 计算与审计记录
- 不改变摘要行的 protected/User 语义
- 不改动 octos-agent 传统通道(tier-3 FullCompactor)的行为

## Out of Scope

- 保留窗口按语义选取(中断回合尾巴降权)——后续独立合约
- 最新用户消息前注入独立 synthetic 标记行(涉及 prompt bridge 覆盖窗口,
  先以摘要尾部声明达成,观察效果后再决定)

## Completion Criteria

Scenario: 摘要生成提示词包含背景降级指令
  Test:
    Package: octos-agent
    Filter: llm_compaction_prompt_demotes_history_to_background
  Given LLM 压缩摘要的系统提示词
  When 检查其文本
  Then 包含"背景/background"降级声明
  And 包含"最新用户消息优先/newest user message"声明

Scenario: 压缩后的 prompt 摘要行带头尾背景框架
  Test:
    Package: octos-cli
    Filter: compaction_summary_renders_with_background_framing
  Given 一个执行过 compact_context 的 ContextManager
  When 调用 for_prompt 组装 prompt
  Then 摘要行头部包含背景声明
  And 摘要行尾部包含"最新用户消息是当前指令"声明
  And 摘要行仍为 protected 的 User 角色

Scenario: 未压缩会话不出现背景框架
  Test:
    Package: octos-cli
    Filter: uncompacted_prompt_carries_no_background_framing
  Given 一个从未压缩的 ContextManager
  When 调用 for_prompt
  Then 输出不包含背景框架文本

Scenario: 框架不进入存储与哈希
  Test:
    Package: octos-cli
    Filter: framing_is_render_only_and_leaves_transcript_hash_stable
  Given 同一个已压缩的 ContextManager
  When 比较存储的摘要 item 文本与渲染输出
  Then 存储的 summary 原文不含框架文本
  And 连续两次 for_prompt 不改变 transcript hash
