# e2e TESTS-KEPT

本文件是后续瘦身的保留清单。测试名使用 Playwright 完整标题（含 `describe` 的 ` › ` 前缀）或 `node:test` 的准确名称。T-0562 仅对齐已经退休的 DOT 产品合同；这些删除不是 ratchet-offset，也不表示通用后台交付、监督或失败恢复合同被删除。退休依据与替代路径见 [DOT pipeline retirement](../docs/DOT_PIPELINE_RETIREMENT.md)。

## 基线与核名

- 基线：`e85ab2219`。`test-lines.py` 实测全仓 93,215 行 / 2,095 项，`e2e` 为 **17,051 行 / 243 个静态计数**。
- `NO_COLOR=1 npx playwright test --list`：默认安全集 **71 tests / 22 files**。
- 使用仅含 `**/*.spec.ts` 与 `**/*.property.ts`、不清空 live 集的临时 list config，并以无效占位 token 避免真实连接：实际发现 **164 tests / 54 files**。
- `node --test tests/ux/*.test.mjs tests/matrix/*.test.mjs`：实际枚举 **49** 个 Node 测试；静态行数/计数与运行时发现是不同口径，不混用。
- 原始清单保留 **50** 项。T-0562 在 #2691 / #3044 已退休合同的基础上删除 3 项；T-0582 第二轮合并 1 个相同 cursor 错误分支，现保留 **46** 项：类别 1 为 7、类别 2 为 7、类别 3 为 7、类别 4 为 21、类别 5 为 4。
- 合同退休删除：`M8 pipeline end-to-end › per-node tasks register under the run_pipeline parent task`（DOT 节点父任务链）、`text-only IR run_pipeline reaches the user as a background completion`（IR 后台完成生产者）、`restart-from-node re-runs only the failed subtree`（DOT 节点缓存/子树重跑）。通用 spawn 后台完成、任务监督与失败恢复仍由下列替代路径覆盖。
- 主机、trial、参数化错误变体不因“文件可能有用”而保留。

## 类别 1：真实跨系统用户链路

| 测试名 | 来源 | 一句话守护性质 |
|---|---|---|
| `first-install admin setup wizard rotates token and persists completion` | `tests/admin-setup-wizard.spec.ts` | 首装浏览器流程真实轮换管理员令牌并持久化完成状态。 |
| `Live browser smoke › short TTS success renders exactly one audio attachment` | `tests/live-browser.spec.ts` | 从用户发起到浏览器音频附件的真实 TTS 交付链不重复、不丢失。 |
| `MoFA full-flow: install → exercise → remove › install mofa skill, exercise all features, then remove` | `tests/live-mofa-full-flow.spec.ts` | 第三方 MoFA 技能安装、全功能调用和移除的完整用户生命周期可用。 |
| `Harness M4.4: third-party skill compatibility gate › install-run-reload-remove cycle proves harness compatibility` | `tests/skill-compat-gate.spec.ts` | 通用第三方技能跨安装、运行、重载、移除保持 harness 兼容。 |
| `M7.8 live swarm dispatch gate › dispatches three sub-agents and emits progress` | `tests/swarm-dispatch-gate.spec.ts` | 真实 swarm 分派多个子代理并把进度传回用户界面。 |
| `Live deliverable flows › slides flow renders one final deck artifact after reload` | `tests/live-slides-site.spec.ts` | 幻灯片跨生成、交付和浏览器重载后仍只有一份最终 deck。 |
| `Phase 3 coding hard cases › coding fanout completes bounded child sessions and delivers their results` | `tests/coding-hardcases.spec.ts` | 编码 fanout 的子会话有界完成并把结果交回父用户链路。 |

## 类别 2：跨功能相互作用

| 测试名 | 来源 | 一句话守护性质 |
|---|---|---|
| `Live browser smoke › research podcast delivers exactly one audio card after reload` | `tests/live-browser.spec.ts` | deep research、podcast 与重载恢复交叉时仍只交付一张音频卡。 |
| `full tool chain: chat triggers activate_tools → shell → ffmpeg` | `tests/tool-use-regression.spec.ts` | 聊天、动态工具激活、shell 与 ffmpeg 的跨功能调用链真实闭环。 |
| `M8 cost tracking › plugin v2 cost_attribution events surface on supervised tasks` | `tests/live-cost-tracking.spec.ts` | 原生 search 经 spawn 后台执行时，plugin v2 与 supervised task 的成本归因事件仍能跨层展示；不依赖 DOT 节点。 |
| `Background task lifecycle › regular messages work while TTS runs in background` | `tests/runtime-regression.spec.ts` | 后台 TTS 与普通聊天并行时两条功能链互不阻塞。 |
| `Phase 3 coding hard cases › failing test is repaired without starting a second ghost turn` | `tests/coding-hardcases.spec.ts` | 测试修复与 turn 去重交叉时不产生第二个幽灵 turn。 |
| `M8.10 stress: thread_id binding under realistic 3-10 user overflow › stress scenario: media-mix-soak-five-skills` | `tests/live-overflow-stress.spec.ts` | 五种技能与溢出并发交叉时仍保持消息和交付绑定。 |
| `Live browser smoke › deep research survives reload without ghost turns` | `tests/live-browser.spec.ts` | 原生 search 替代路径与浏览器重载交叉时不重复创建 turn，也不恢复已退休 DOT 工具。 |

## 类别 3：安全、权限与隔离

| 测试名 | 来源 | 一句话守护性质 |
|---|---|---|
| `workspace validators match existing, missing, and root-escape probe shapes` | `tests/matrix/run-validators.test.mjs` | UX 证据校验能识别 root-escape 探针形状；它是证据校验边界，不冒充生产文件系统渗透测试。 |
| `capability ordering and unadvertised method calls fail` | `tests/ux/validate.test.mjs` | 未声明方法调用与 capability 顺序违规会被验证器明确拒绝。 |
| `unredacted retained secrets fail secret_redaction` | `tests/ux/validate.test.mjs` | 留存证据中的明文 API key 必须触发脱敏失败而非通过验收。 |
| `Cross-session isolation › concurrent sessions do not leak messages (#343)` | `tests/runtime-regression.spec.ts` | 并发真实会话之间不泄漏用户消息。 |
| `M9 protocol — fault injection › session/open with an unknown SessionRef does not fall back to session_id` | `tests/m9-protocol-fault-injection.spec.ts` | 未知 SessionRef 不得回退为可猜测的 session_id 而错误绑定会话。 |
| `M9 protocol — fault injection › session/open rejects an unconfigured profile in the key or params` | `tests/m9-protocol-fault-injection.spec.ts` | key 或参数中的未配置 profile 都被拒绝，避免跨 profile 混淆。 |
| `M8.6 Resume sanitizer worktree-missing refusal › deleting workspace mid-session forces a hard refusal on next turn` | `tests/m8-runtime-invariants-live.spec.ts` | 会话工作区消失后下一 turn 必须失败关闭，不能在未知路径继续写入。 |

### 第 3 类前固定安全边界评审行（索引，不额外增加保留数）

| 安全子类 | 对应测试或缺席理由 |
|---|---|
| 路径穿越 | `workspace validators match existing, missing, and root-escape probe shapes`；覆盖 root-escape 证据识别，生产路径穿越防线仍由 Rust 文件/沙箱测试承担。 |
| 沙箱逃逸 | **本 crate 没有直接沙箱逃逸负例**：`ffmpeg concat works in sandbox workdir` 只证明沙箱内正常执行，不能冒充逃逸测试；真正的逃逸边界由沙箱后端测试承担。 |
| 鉴权/权限 | `capability ordering and unadvertised method calls fail`；另由未知 SessionRef、未配置 profile 两条 M9 负例核对会话绑定权限。 |
| 密钥/令牌脱敏 | `unredacted retained secrets fail secret_redaction`。 |
| 外部输入大小上限 | **本 crate 没有直接请求字节/令牌上限测试**：overflow 场景验证时序和 thread binding，不是外部输入大小拒绝；该边界由协议解析和工具参数测试承担。 |

## 类别 4：恢复、数据、并发与协议帧

| 测试名 | 来源 | 一句话守护性质 |
|---|---|---|
| `M9 protocol — fault injection › drop mid-stream and reconnect with last cursor: replay restores state without duplicates` | `tests/m9-protocol-fault-injection.spec.ts` | 断线后按 cursor 重放恢复完整状态且不重复。 |
| `M9 protocol — fault injection › future cursor beyond a retained ledger head returns typed cursor_expired` | `tests/m9-protocol-fault-injection.spec.ts` | 有留存事件的 session 用远超 ledger head 的 cursor 恢复，得到稳定的 `CURSOR_OUT_OF_RANGE / cursor_expired`；合并原 stale/future 两个相同分支，保留较强的有留存事件代表。 |
| `M9 protocol — fault injection › double turn/start with the same turn_id: idempotent accept or typed error` | `tests/m9-protocol-fault-injection.spec.ts` | 重复 turn/start 只能幂等接受或 typed error，不能执行两次。 |
| `M9 protocol — fault injection › double approval/respond returns -32011 approval_not_pending` | `tests/m9-protocol-fault-injection.spec.ts` | 重复审批响应不能重复执行已消费的 approval。 |
| `M9 protocol — fault injection › double turn/interrupt drains to one interrupted terminal event` | `tests/m9-protocol-fault-injection.spec.ts` | 重复 interrupt 最终只排放一个 interrupted terminal。 |
| `M9 protocol — fault injection › slow client receives protocol/replay_lossy after durable notification drops` | `tests/m9-protocol-fault-injection.spec.ts` | 慢客户端在持久通知丢弃后收到显式 replay_lossy，而非误以为完整。 |
| `M9 protocol — session/append_message › retry with the same client_message_id returns the original seq` | `tests/m9-protocol-session-append.spec.ts` | 追加消息重试保持幂等并返回原始序号，避免重复持久化。 |
| `M9 protocol — session/open › resume with a cursor from a different stream returns cursor_stream_mismatch` | `tests/m9-protocol-session-open.spec.ts` | 不同 stream 的 cursor 不能串用恢复会话。 |
| `M9 protocol — task/output/read › initial read + follow-up read advance by task/output cursor` | `tests/m9-protocol-task-output-read.spec.ts` | task/output 多次读取按 cursor 正确前进且不重复。 |
| `M9 protocol — tool events › tool_start -> tool_end envelope correlation by tool_call_id` | `tests/m9-protocol-tool-events.spec.ts` | 工具起止协议帧必须通过同一 tool_call_id 关联。 |
| `M9 protocol — turn/start › happy path: turn/started -> assistant_delta+ -> turn_terminal envelope` | `tests/m9-protocol-turn-start.spec.ts` | 正常 turn 的 started、delta、terminal 帧序列完整可达。 |
| `Live session recovery › reloading twice during an active stream resumes the same turn` | `tests/session-recovery.spec.ts` | 活跃流连续重载两次仍恢复同一 turn，不重启用户任务。 |
| `M8 spawn end-to-end (slides) › failed spawn child triggers M8.9 recovery prompt` | `tests/live-spawn-end-to-end.spec.ts` | spawn 子任务迟到失败必须转成可操作的恢复提示。 |
| `Phase 3 coding hard cases › long idle resume keeps the same coding turn after reconnect` | `tests/coding-hardcases.spec.ts` | 长空闲和重连后仍恢复原编码 turn。 |
| `thread_id binding property › generated speculative-overflow transcripts preserve binding across 50 cases` | `tests/property/thread-binding.property.ts` | 50 组生成溢出 transcript 的 thread_id 绑定不变量持续成立。 |
| `Live overflow thread binding (M8.10 follow-up #649) › 3-user fast follow-up keeps each bubble paired with its own assistant` | `tests/live-overflow-thread-binding.spec.ts` | 三用户快速追问下每个 bubble 仍与正确 assistant 配对。 |
| `Session create & delete lifecycle › deleted session workspace files are cleaned up` | `tests/runtime-regression.spec.ts` | 删除会话后其工作区文件真实清理，避免残留数据。 |
| `rpc() on a backend that exits with pending requests fails those requests too` | `tests/matrix/run-stdio-client.test.mjs` | 后端退出时所有悬挂 RPC 都收到失败，不能永久等待。 |
| `reconnect events without a post-restart event fail the restart contract` | `tests/ux/restart-validate.test.mjs` | 只有 reconnect 而没有重启后事件的证据不能通过恢复验收。 |
| `fixture-only replay-lossy report does not satisfy dropped-completion contract` | `tests/ux/backpressure-validate.test.mjs` | 伪造 replay-lossy 文本不能替代真实 terminal-drop 证据。 |
| `M7.8 live swarm dispatch gate › preserves swarm state after protocol and browser reload` | `tests/swarm-dispatch-gate.spec.ts` | swarm 状态跨协议与浏览器重载仍保持可恢复。 |

## 类别 5：有意设计的合同

| 测试名 | 来源 | 一句话守护性质 |
|---|---|---|
| `M15 AgentOrchestrator contract docs › pin the backend-owned orchestrator replacement contract` | `tests/m15-agent-orchestrator-contract.spec.ts` | 固定 M15 后端拥有 orchestrator 替换合同，防止多宿主实现漂移。 |
| `M15 AgentOrchestrator contract docs › requires native, CLI, and MCP coverage in acceptance and soak evidence` | `tests/m15-agent-orchestrator-contract.spec.ts` | 按 M15 设计要求 native、CLI、MCP 三类宿主都必须有验收与 soak 证据。 |
| `manifest has at least one stdio and one ws transport scenario` | `tests/ux/scenario-list.test.mjs` | 按 matrix 双传输设计，清单必须同时覆盖 stdio 与 WebSocket。 |
| `checked-in capability gate leaves unproven M19 lanes blocked` | `tests/ux/scenario-list.test.mjs` | 按能力门控设计，未被 checked-in 证据验证的 M19 lane 必须保持 blocked。 |

## 第二轮去重与共享 setup（T-0582）

- 原 `stale (zero-seq) cursor against a session with retained events surfaces cursor_expired` 实际使用 `openedSeq + 100_000`，与 `future cursor (beyond head) returns typed cursor_expired` 走同一错误分支；保留有留存事件的较强代表，改名为 `future cursor beyond a retained ledger head returns typed cursor_expired`，删除另一个无留存事件重复场景。
- `tests/m9-protocol-fault-injection.spec.ts` 新增普通 async helper `withSession(prefix, run, env?)`，集中 `liveServerEnv → M9WsClient → uniqueSessionId → try/finally close`；5 个单客户端场景复用它，双连接重连/cursor 场景保留显式两客户端生命周期。没有用宏隐藏测试或降低断言。
- 第 3 类五子类、跨功能、失败/迟到、取消、恢复、兼容与平台条目均保留；未改 transport-test-fixtures 非默认隔离、required lane 或任何生产接口。

## 评审优先指针（索引，不额外增加保留数）

- **两个功能交叉**：优先问 `Live browser smoke › research podcast delivers exactly one audio card after reload` 是否仍同时证明 deep research → podcast → reload，而不是三个孤立断言。
- **失败/迟到路径**：优先问 `M8 spawn end-to-end (slides) › failed spawn child triggers M8.9 recovery prompt` 是否仍覆盖子任务失败后的用户恢复，而非只验证 happy path。
