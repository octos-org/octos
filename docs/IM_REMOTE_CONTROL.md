# IM 遥控设计：serve 接入消息总线

日期：2026-11-16。状态：**设计（只写文档，不写产品代码）**。作者：dev-web-kimi（T-0632）。
上游：`docs/octoscode-web-vnext/plan.md` §6 N2；调研基础 T-0093（现状盘点与 C→A 推荐，已定：C 先行、B 暂缓、A 的首渠道由 shu 定）。

> **前提假设（合入时都未合）**
>
> - T-B：每 Chat 一个服务端创建的 workspace——按 [#3256](https://github.com/octos-org/octos/pull/3256) 当前头 `aa2059fe` 的契约写：`session/chat/create`（feature `session.chat_workspace.v1`，幂等）、`<profile data_dir>/chats/<sha256(session_id)[..32]>/`（0700）、chat 默认 `read_only` + `network: deny` 且永远得不到 `danger_full_access`/网络放行（`chat_permission_bound`）、绑定随 session 删除、文件 API 限定在 chat 文件夹。若 #3256 改契约，本文 §5 需要跟着改。
> - chat→profile 显式映射：按 [#3297](https://github.com/octos-org/octos/pull/3297) 当前头的语义写——channel entry 的 `chat_profile_map` / `guild_profile_map`，Discord channel 命中优先于 guild 兜底，显式映射未解析时 loud 拒绝（`profile_route_explicit`），无映射保持单 profile。
> - T-0621 真核审计发现现有 `permission/profile/set` 有越权/重启丢失缺口（F01 跨 profile 越权、F02 重启丢显式只读、F03 host-managed 广告但拒绝），已拆 T-0625/T-0626/T-0627。**IM 设计不依赖 SET 放宽**：高风险操作一律跳 PWA（§8）。

## 0. 结论

推荐 T-0093 的**方案 A**：serve 直接复用 `octos-bus` 的渠道实现，新增 `ServeImBridge` 作为另一种 ingress，消息经**显式 binding** 路由到既有 `(profile, session, workspace)`，调用从 OUP turn/open/interrupt/approval 处理中抽取的内部 `SessionTurnService`。**不在 serve 里起第二套 `SessionActor`**，不用假 WebSocket 调 dispatcher，不复用 gateway 的内存审批。方案 B（远端 gateway 配对）继续暂缓。

第一个可独立合入的小 PR：`octos-cli` 的 `SessionTurnService` 抽取 + 单测，不接任何真实渠道（§14）。

## 1. 组件边界

```
Telegram/Discord/... ── octos-bus Channel（复用实现）
        │ InboundMessage
        ▼
ServeImBridge（新增，crates/octos-cli）
  ├─ binding 路由（§4）           ← 每次 inbound 重查
  ├─ 持久 inbox / outbox（§9）    ← 落盘后才执行
  └─ 调用 SessionTurnService ──────┐
                                   ▼
SessionTurnService（从 OUP handlers 抽取的内部服务，新增）
  = session/open · turn/start · turn/interrupt · approval/respond
                                   │
                                   ▼
                 既有 SessionRuntime / PendingApprovalStore / ledger
                 （OUP WebSocket 客户端与之并行，互不知晓）
```

- **与 OUP 的关系**：OUP 的 WS handlers 变成 `SessionTurnService` 的一层薄适配；`ServeImBridge` 与浏览器客户端**同级**，都只是 ingress。协议可见面（方法名、事件形状）不变；IM 侧没有新 OUP 方法（§12）。
- **与 SessionRuntime 的关系**：每个被 IM 驱动的 session 就是普通 serve session，走 `SessionRuntimeCache` 同一缓存；IM 不创建也不持有 runtime。cancel/interrupt、恢复、ledger cursor 全部继承。
- **与 gateway 的关系**：同一渠道要么 gateway 要么 serve 消费，绝不同刻双开（§7 单 owner）。gateway 的 `SessionActor`/内存审批不迁移、不共享。
- **不做什么**：不改 `turn/start` 语义，不在 IM 里实现"永允许"，不引入 push 服务（首版 IM 渠道自己的出站消息就是通知通道），不碰模型/技能/连接器配置。

## 2. 身份与配对

- **binding 先行**：运维在桌面端（或配置文件）创建 binding：`{channel, platform_sender_id, chat_id, profile_id, session_ref}`。binding 是租户归属的唯一权威；**IM 用户消息不能自选任意 profile**（§4）。
- **一次性确认码**：binding 创建后生成短码（复用 `session_ingress` 的 work-secret 思路：短期、可撤销、绑定 scope）。sender 在对应 chat 发 `/start <code>` 完成确认；码错/过期/重放拒绝并计入审计。IM 全程不接触模型 API key——渠道 bot token 只在服务端 secret store。
- **渠道 allowlist 必须非空**：`allowed_senders = []` 在 gateway 里是 allow-all（T-0093 已点名），serve 侧**反转为必须显式列出**；空表 = 该渠道在 serve 不启用。
- **撤销**：binding 删除/禁用即刻生效（下次 inbound 重查时拒绝）；进行中的 turn 不打断，但新消息与审批响应都被拒。

## 3. 普通消息 / callback / thread 的形状

- **普通文本消息** → binding 命中后成为 session 的用户输入（走 `SessionTurnService.turn_start`）。
- **命令**：首版只认 `/start`（配对）、`/stop`（interrupt 当前 turn）、`/status`（只读状态）。斜杠命令解析在 bridge 内完成，解析失败 loud 回复，绝不把未知命令当 prompt 发给模型。
- **callback query（Telegram 按钮）**：只用于审批（§8）；callback 与对应消息共用同一 stamp 点（沿用 #3297 的做法），非审批 callback loud 拒绝。
- **thread/话题**：Telegram topic 与 Discord thread 映射到 binding 的 `chat_id`；Discord thread 先查自身映射，再落 parent channel，再落 guild（与 #3297 的 channel>guild 一致的 thread>parent>guild 顺序）。**映射只决定 profile/session 归属，不产生新 session**——session_ref 在 binding 里显式给出。
- **附件**：图片/文件入站按 serve 现有上传语义落进该 session workspace 的 `uploads/`；出站文件经 `send_file`（#3256 的 chat 分支限定只能从 chat 文件夹发出）。超大小/不支持类型 loud 拒绝。

## 4. 显式映射与未知目标

- 入站先查 binding：`(channel, chat_id[, thread_id], sender_id)`。顺序：thread > chat > channel/guild 兜底（guild 兜底仅 Discord，沿用 #3297 语义）。
- **未知目标一律拒绝**：无 binding、binding 已撤销、sender 不在 allowlist、映射目标 profile 不存在——都 loud 拒绝（渠道内回复"未授权"），**不 fallback 到 main**。与 #3297 的 `profile_route_explicit` 同一原则：启发式猜测不出现在 serve 侧。
- 每条被接受的消息在 session ledger 留下 `{channel, chat_id, sender_id, platform_message_id}` 来源记录（供审计与 §9 幂等）。

## 5. 每 Chat workspace 归属（依赖 T-B）

- IM 发起的对话是 chat：binding 的 `session_ref` 指向 `session/chat/create` 创建的 chat session，workspace 由服务端创建、绑定、0700、随删除（#3256 契约）。**IM 不提供 cwd 选择**——chat 的文件夹是服务器的，不是用户报来的路径。
- chat 的权限默认值由 #3256 保证（read_only + deny，永远得不到 full access/网络放行）。IM 侧不需要也不允许改权限（§8：高风险跳 PWA）。
- T-B 合入前本节的实现不启动；binding 可以引用既有 code session（§4 的 session_ref 允许普通 session，但那是运维显式选择，不是 IM 用户选择）。

## 6. 群聊与 sender 授权

- 群聊默认拒绝；只有 binding 同时命中群 chat_id **和** sender_id 才受理。
- 群内其他人的消息不进入 session；群内审批响应只认 binding 里的 sender（§8 的 ownership）。
- 同一 chat_id 可被两个不同 bot 实例映射到不同 profile（沿用 #3297 的实例独立语义）。

## 7. 渠道单 owner

- serve 与 gateway 子进程**不得同时轮询/消费同一渠道账号**。引入渠道 owner 声明：渠道配置带 `owner: "gateway" | "serve"`，启用 serve IM 的渠道在 gateway 注册表中被拒绝启动（反向亦然）；owner 判定在渠道启动时 fail-fast。
- 配置只有一个来源（见 §11 迁移）；运行时再用一个每渠道文件锁防双进程互踩。

## 8. IM 里的审批

- **复用真实 approval UUID 与 ownership**：审批事件来自 `PendingApprovalStore`（serve 现有），bridge 投影为渠道消息（有按钮用按钮，没有则文本 fallback：回复 `yes <nonce>` / `no <nonce>`，nonce 为短随机、一次性、与 approval_id 绑定）。
- **单次 approve/deny**：响应消费后该 approval 即终态；重放（同 nonce 再来、旧消息重发）拒绝。响应必须来自 binding 的 sender+chat，且 approval_id 属于该 session——每次响应重查 binding、撤销状态与 approval 当前状态。
- **过期**：审批消息带 TTL；过期后 IM 响应拒绝（服务端 `PendingApprovalStore` 的终态判定为准），需要时在 PWA 里重新发起。
- **撤销**：turn 被 interrupt 或 session 关闭时，bridge 把未决审批消息标记为已失效（渠道允许则编辑原消息）。
- **高风险一律跳 PWA**：改权限档、开放网络、workspace 外写入、凭据/连接器管理，IM 永不提供入口，只给"请在浏览器里处理"的指引。这同时避开 T-0621 的 F01–F03 缺口：IM 路径零 SET。
- **未知结果**：审批响应发出后渠道 ack 失败/超时 → 标记"结果未知"，重查服务端状态再展示，绝不盲重发"已批准"。

## 9. 离线、幂等与恢复

持久 inbox/outbox **不是**执行/发送的幂等保证；它们只是记录。两个崩溃窗口必须显式处理，重启一律先对账、后动作。

### 9.1 入站崩溃窗口：pending 可能已 dispatch

一条消息的执行链条是"inbox 落盘 → 调 `SessionTurnService`（服务端产生 `turn_id` 并写 ledger）→ 标记 dispatched"。崩溃可能落在任何一环，而"已落盘、未标记 dispatched"**不等于**"没执行"。规则：

- inbox 落盘时立即记录状态机：`received → dispatching（先落本次 dispatch 的目标 session 与请求摘要）→ dispatched(turn_id) → terminal`。
- `dispatching → dispatched` 的推进以拿到服务端 `turn_id` 为准，拿到就先落盘再继续。崩溃在"dispatching 无 turn_id"：重启**不得盲放执行**——先按 ledger 对账（该 session 最近是否有同摘要/同时窗的用户输入），对得上则记 `dispatched(turn_id)`，对不上且不确定则标记 `unknown` 并人工处理（loud 提示，不重放）；只有确定未 dispatch（ledger 无迹）才允许重放执行。
- 重复投递（渠道重试）只挡"同 `platform_message_id` 已有 terminal/已 dispatch 记录"这一层；对 unknown 不自动重发。

### 9.2 出站崩溃窗口：ack 丢失 ≠ 未送达

渠道**没有通用客户端幂等键**：Telegram 的业务方法不接受客户端提供的 dedupe key，Discord 同理；reply/edit id 只能在消息已存在后定位它，不能保证"ack 丢失后重发不产生新消息"。规则：

- outbox 记录 `{目标, 内容摘要, 状态: pending/sent/failed/unknown}`；`sent` 只由渠道明确 ack 推进。
- 发送请求发出后 ack 丢失/超时 → 状态 `unknown`，**不自动重发**。按渠道能力对账：能按 reply/edit 锚点或 `getUpdates` 回读确认已送达的，确认后改 `sent`；渠道无回读能力的（Discord 无回溯）保持 `unknown` 并 loud 提示人工核对。
- 恢复重连只补发"从未发出过"（pending 且未尝试）的通知与结果；`unknown` 一律不自动补发。**过期命令与过期审批一律不自动执行/重发**。
- TTL 过期的 inbox 条目不执行（loud 回复"已过期"）；过期的 outbox 条目标记丢弃。

### 9.3 恢复与存储

- **断线恢复**：bridge 重启后先跑 §9.1/§9.2 的对账，再恢复消费平台 cursor。Telegram 用 `getUpdates` offset 续拉；Discord 不支持回溯则明确丢弃缺口并 loud 说明，符合"恢复只补状态"语义。
- **serve 离线**：渠道侧错误由渠道自身表面呈现；serve 恢复后 bridge 按上面对账。
- 现有 mpsc 内存 bus 不承担这些保证（T-0093 已点名）；inbox/outbox 用 sqlite 或 append-only 文件，跟 profile 数据目录走。

## 10. 失败与未知结果

- turn 启动失败 → loud 回复失败原因（不静默吞）。
- turn 结果不明（连接断在服务端已写入之后）→ 展示"不确定"，恢复时从 ledger 读回终态再展示；绝不重复提交同一用户消息（幂等键挡）。
- 渠道发送失败与 serve 内部失败分开记账；用户可见的只有"已送达/失败/未知"。

## 11. 配置与迁移

- 新配置段 `serve.im`（渠道引用 + binding 列表）。迁移规则：已有 gateway 渠道 entry 默认 owner=gateway，行为不变；运维显式把 `owner` 改为 serve 并补 binding 才启用。`chat_profile_map`/`guild_profile_map`（#3297）在 owner=serve 时由 bridge 读取同一份配置，语义不变。
- 配置错误 loud：`owner: serve` 但无 binding / allowlist 为空 / binding 指向不存在的 profile——渠道启动即拒绝。

## 12. UPCR 与协议边界

- 首版纯内部桥：**不要求新 OUP 方法/事件**，因此不需要 UPCR。
- 若后续新增 `channel/*` OUP 方法、远程 binding 管理或移动审批推送，必须先走 UPCR。
- IM 投影所用的事件（turn 状态、审批、文件产出）全部来自既有 ledger/projection 事件，不发明新 wire 形状。

## 13. 测试矩阵

| 层 | 覆盖 |
| --- | --- |
| 单元（bridge） | binding 命中/未命中/撤销；thread>chat>guild 顺序；allowlist 空=拒绝；群聊非授权 sender 拒绝；未知命令 loud；`platform_message_id` 重复投递去重；TTL 过期不执行；§9 两个崩溃窗口——dispatching 无 turn_id 时按 ledger 对账（确定未 dispatch 才重放，不确定转 unknown 人工）、outbox `unknown` 不自动重发、按渠道能力对账。 |
| 单元（SessionTurnService） | 抽取后与 OUP handler 行为一致（同一 fixture 双跑）。 |
| 集成（假渠道） | 内存 fake Channel 驱动真实 serve：消息→turn→审批（同意/拒绝/过期/重放/非授权 sender）→结果出站；高风险只出 PWA 指引；chat workspace 落位（T-B 合后）。 |
| e2e（真实渠道沙盒） | 首渠道确定后再排：一个测试 bot + 测试群的全链路。 |
| 回归 | gateway owner=serve 的渠道在 gateway 拒绝启动；gateway 行为在 owner=gateway 下不变。 |

## 14. 第一刀小 PR

**`SessionTurnService` 抽取（octos-cli，纯内部重构）**：把 OUP `session/open`·`turn/start`·`turn/interrupt`·`approval/respond` 的处理核心从 WS handler 中抽出为无 I/O 依赖的内部服务，WS handler 改为薄适配。带"同一 fixture 双跑"单测证明行为不变。**不接渠道、不改配置、不动 gateway**——可独立评审合入，为 bridge 铺路。

实际落地（T-0636）：服务在 `crates/octos-cli/src/api/ui_protocol_session_turn_service.rs`，经 `TurnCaller` 认识调用方（连接 id、external、work-secret 代际、durable 事件投递、turn runner），不碰 WebSocket 或 JSON-RPC 帧；WS 与 stdio 的 handler 都是它的薄适配。仍留在 handler 的只有传输本身的事：`session/open` 的订阅、replay 与 live forwarder，以及 `turn/start` 先回 accept 再放行 turn。

已准入的 turn 仍由调用方的 runner 执行；WebSocket 的 runner 是 `run_standalone_turn`，它在约 70 处直接向自己的连接投递。bridge（T-0637）需要一个不经 socket、只写 ledger 的 runner，这是下一刀，不能用"分离的 `WsConnection`"凑。

第二刀 a（T-0637，按 T-0633 评审的 B1/B2/B5/B6 收窄）：

- **runner**：`run_standalone_turn` 和它调用的发送 helper 改为经 `TurnConnection` 投递（`crates/octos-cli/src/api/ui_protocol_turn_connection.rs`）。`WsConnection` 照旧直送自己的客户端并写 ledger；`LedgerConnection` 没有客户端，turn 的持久事件只经 session ledger 到达读者。
- **内部入口 `ExternalTurns`**（`ui_protocol_external_turns.rs`）：
  - 每个调用都带 `BoundChat`：`BoundProfile` 是本 server 服务的 profile；session 必须是这个 profile 的、已存在且可用的 Chat，带 profile 前缀、不带 topic。Code session 不能绑定。没有 `Option` profile，不退到 `_main`，不跨 profile 查找，也不改开别的 session。
  - turn_id 由调用方给。`start` 先在 profile 数据目录（`external_turns/`）落盘这个 id，再启动 turn；同一 id 再来时，返回第一次的结果，不再执行。重启后 `reconcile` 按精确 turn_id 报告每个已记录的 turn：未开始、运行中、已结束，或被重启打断（ledger 重载时补的 `orphaned_by_restart`，不重跑）。不按摘要或时间窗猜测。
  - turn 是 external（UPCR-2026-036）：只保留 external 工具白名单；别处授予的 session 级批准对它不生效；它只能停、只能批自己的 turn 和审批，批准时请求的 scope 一律丢弃。
- 还没有调用方。fake Channel 绑定核心是 T-0653，配置和单 owner 是 T-0654；附件、IM 审批 UI、持久 outbox 都不在这一刀。

## 15. 待 Claude / shu 定（本文不擅自决定）

1. **首渠道**：Telegram（实现最简）还是飞书/企业微信（国内用户主阵地，WorkBuddy 对标）——T-0093 已明确此项归 shu。
2. binding 管理面：首版只读配置文件，还是要管理 UI/命令（后者牵动 UPCR 边界）。
3. 审批 TTL 的具体数值与渠道消息编辑能力的取舍。
4. Discord 无回溯 API 时的断线语义是否接受"丢弃+loud 说明"。
