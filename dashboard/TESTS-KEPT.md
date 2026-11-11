# dashboard TESTS-KEPT

> **阶段 1：仅保留清单。** 本 PR 不删除测试、mock、fixture 或生产代码。必须经 lead-core 验收、正常维护者评审并实际合入，之后才能另开唯一的 dashboard-wide 删除 PR。

基线：`main@7c16f6c3b1bff2cec9a64d984d9fd33f0a40cbac`。
`python3 wake/test-lines.py --json`：octos **87,915 行 / 1,994 测试**；dashboard **2,441 行 / 85 个脚本计数测试**。
Node `v22.23.2` 下 `npm test -- --reporter=verbose`：**17 files / 87 tests passed**。两个 `it.each` role case 展开后不被 test-lines 脚本计数，因此机器清单以 **87 个真实 Vitest 名称**对账，85 只用于同口径行数比较。

类别：**1** 跨系统用户链路；**2** 跨功能交互；**3** 安全/授权/边界；**4** 恢复、数据、并发与协议正确性；**5** 有意设计决定。
拟保留 **58 项**，逐项核对组件→context→adapter/fetch→状态/导航链；不保留逐 prop、逐文案或同路径变体。API 均为离线 mock，证明前端调用组合，不冒充 Rust 服务端授权集成。

## 第 3 类：固定五个安全子类

| 安全子类 | 准确测试名，或无边界理由 | 守护的真实行为与限制 |
|---|---|---|
| 路径穿越 | **dashboard 无 filesystem/canonicalization 边界。** 前端不解析宿主文件路径；WeChat URL segment encode 只作为协议完整性保留，不冒充 `..`/symlink 防护。 | 真正的文件/根目录边界属于 agent/server。 |
| 沙箱逃逸 | **dashboard 无此边界。** 前端不 spawn 进程、不执行 OS sandbox，也不解析宿主文件系统。 | OS/process sandbox 属于 agent/server；不以 jsdom 测试冒充。 |
| 鉴权/权限 | **无服务端授权测试。** 所有 API/Auth 均为 mock；保留的 UI role、host-scope、profile route、destructive-confirm 只证明前端分支，不证明 admin/user middleware 或 foreign-profile deny。 | 真正授权属于 Rust router/solo_auth；此处明确 UI 与 server 的边界。 |
| 密钥/令牌脱敏 | **无合格脱敏测试。** WeChat confirm 的 mock 不含 bot_token，只能证明 config 合并，不能捕捉响应泄密；SSE 又把 token 放入 query string。 | 不把 password input、token 不落 config 或空 mock 的负断言当成日志/持久化脱敏。 |
| 外部输入大小上限 | `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > keeps the submit button disabled until the operator types exactly 8 characters` | 7/8/9 字符 UI 边界：只有精确 8 可提交，9 也拒绝。仅是客户端 token 规则；服务端最小长度与 1 MiB body cap 不在此测试范围。 |

## 登录、授权与 setup

| 测试名 | 类别 | 守护性质 |
|---|---:|---|
| `src/contexts/AuthContext.test.tsx > AuthContext soloCreate > sets the user from the create result even when /me fails` | 1 | soloCreate 成功但 `/me` 失败时仍建立 principal 并保存 session token，避免新用户被弹回 login。 |
| `src/contexts/AuthContext.test.tsx > AuthContext soloCreate > refines the user from /me when it succeeds` | 2 | create result 与 `/me` canonical principal 合并，成功刷新用户身份。 |
| `src/pages/LoginPage.test.tsx > LoginPage solo affordance > hides the solo button when local_solo_enabled is false` | 3 | 非 solo host 不暴露 no-password 入口。 |
| `src/pages/LoginPage.test.tsx > LoginPage solo affordance > logs in the existing owner and navigates home` | 1 | existing owner soloLogin 后进入 home，完成真实登录导航。 |
| `src/pages/LoginPage.test.tsx > LoginPage solo affordance > drops into the create form when no profile exists yet (404)` | 1 | soloLogin 404 转入 create-profile，而非错误停留或部分认证。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > renders children when token is already rotated` | 1 | 已完成 rotation 时不被无条件重定向 setup。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > redirects to /setup/welcome when token not rotated and wizard not completed` | 1 | 首次安装进入 welcome，而非跳过 bootstrap。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > skips welcome and redirects to /setup/rotate-token when wizard already completed but token missing` | 4 | wizard 已完成但 token 丢失时直接进入 re-rotation，避免重复欢迎流程。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > does not redirect to /setup/welcome when token status returns 401` | 3 | 401 交由 AuthGuard 处理，不把未认证用户错误导入 setup。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > treats 500 / network errors as "not rotated" (conservative)` | 4 | ApiError(500) 采用保守 setup 分支，不掩盖 server misconfiguration；标题中的 network 未单独覆盖。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > renders children on a solo host even when the token is not rotated` | 5 | 无密码 solo 无 bootstrap token 可轮换，允许进入受保护页面。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > still forces rotation on a solo-capable host that also exposes admin-token login` | 3 | solo-capable + admin-token 双模式仍强制 rotation，不能借 solo 绕过 live bootstrap token。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > bypasses the gate for non-admin users` | 3 | 非 admin 不执行 admin token/setup 探测，直接按自身范围渲染。 |
| `src/components/Sidebar.test.tsx > Sidebar admin-global gating > hides admin-global links when admin IS host-scoped to a tenant` | 3 | host-scoped admin 不显示全局 admin links，但保留 tenant profile 链接；仅 UI 范围，不是 server auth。 |
| `src/components/Sidebar.test.tsx > Sidebar admin-global gating > non-admin user never sees admin-global links regardless of host scope` | 3 | 普通用户不显示 admin 全局入口。 |
| `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > submits a valid 8-character token and swaps the auth token on success` | 1 | 合法 token 调 rotate API 并同步 AuthContext token，完成轮换链。 |
| `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > shows the API error message when rotation fails` | 4 | rotation 失败显示后端错误，不伪装成功。 |
| `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > trims surrounding whitespace before submitting (login form trims too)` | 1 | rotate/login 两端使用同一 trim 语义，避免保存带空白 token 后锁定用户。 |
| `src/components/SoloProfileForm.test.tsx > SoloProfileForm > keeps submit disabled until name, username and email are valid` | 3 | name/username/email 客户端验证通过前不能发起创建。 |
| `src/components/SoloProfileForm.test.tsx > SoloProfileForm > submits trimmed values and calls onDone on success` | 1 | 创建请求发送 trim 后值并在成功后进入 onDone。 |
| `src/components/SoloProfileForm.test.tsx > SoloProfileForm > surfaces a server rejection` | 4 | server 拒绝创建时显示错误并停留在表单。 |
| `src/pages/NewProfile.test.tsx > NewProfile admin mode shortcut > preselects admin mode from the query string and submits admin config` | 1 | `adminMode=true` 预选并提交 admin_mode/config，完成管理员建 profile 链。 |
| `src/pages/NewProfile.test.tsx > NewProfile admin mode shortcut > leaves admin mode off by default and omits config from the request` | 5 | 默认非 admin 请求不携带 config/admin_mode，保持显式 opt-in。 |

## Profile、导航与真实 API 调用组合

| 测试名 | 类别 | 守护性质 |
|---|---:|---|
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > shows a solo-guard refusal as an error toast instead of "Gateway started"` | 1 | HTTP 200 + `ok:false` 仍显示后端 refusal，不报 Gateway started 假成功。 |
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > falls back to a generic message when a refusal carries no message` | 4 | `ok:false` 无后端 message 时使用 caller fallback，不产生空错误；保留集成行为而非仅 helper 单测。 |
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > still toasts success and refreshes the profile when the action reports ok: true` | 1 | 成功 action 后 toast 并重新读取 profile，保持 UI 与 server 状态一致。 |
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > fails the sub-account two-step restart when only the start leg is refused` | 4 | stop 成功、start 拒绝时 restart 失败，不显示 restarted。 |
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > keeps the admin adapter success path toasting` | 2 | admin adapter 与 self-service adapter 的成功语义不回归。 |
| `src/contexts/ProfileContext.navigation.test.tsx > profile navigation identity > targets the child for stop and save after navigation (admin=true)` | 2 | 导航后 admin stop/save 使用 child identity，不写旧 parent。 |
| `src/contexts/ProfileContext.navigation.test.tsx > profile navigation identity > targets the child for stop and save after navigation (admin=false)` | 2 | 非 admin 导航后 stop/save 同样使用 child identity；共享 spy 只证明 target，不区分 endpoint。 |
| `src/contexts/ProfileContext.navigation.test.tsx > profile navigation identity > targets the child for delete and purge after navigation` | 3 | delete/purge 的破坏性操作在 context 层以 child id 调用；不声称覆盖页面后续导航。 |
| `src/contexts/ProfileContext.navigation.test.tsx > profile navigation identity > clears the previous profile while the new request is pending` | 4 | 新 profile 加载期间清空旧 config，避免跨 profile 数据泄漏/误保存。 |
| `src/contexts/ProfileContext.navigation.test.tsx > profile navigation identity > ignores an old GET that resolves after the child loads` | 4 | 迟到的 parent GET 不覆盖已加载 child。 |
| `src/contexts/ProfileContext.navigation.test.tsx > profile navigation identity > ignores an old save response after navigation` | 4 | 旧 profile save 响应不覆盖新 profile config 或 toast。 |
| `src/contexts/ProfileContext.navigation.test.tsx > profile navigation identity > does not refresh the old profile after a pending gateway action completes` | 4 | 旧 gateway action 完成后不重新加载旧 profile，也不发成功 toast。 |
| `src/contexts/ProfileContext.navigation.test.tsx > profile navigation identity > keeps server-confirmed host scope for the own-profile entry` | 2 | `/my` 返回 host-scoped 后，useProfile().profileId 采用 server-confirmed id，下游 provider/search/subaccount 不停留在 user.id。 |
| `src/pages/profile/MessagingPage.test.tsx > MessagingPage WeChat profile routing > passes the edited profile down when an admin edits another profile` | 2 | 真实 page→tab→fetch 把 admin QR start 发往被编辑 profile；是跨 profile 路由，不是服务端授权证明。 |
| `src/pages/profile/MessagingPage.test.tsx > MessagingPage WeChat profile routing > uses the user-scoped route on the own profile page` | 1 | own profile 的 page→tab→fetch 使用 `/api/my/profile/wechat/qr-start`。 |
| `src/components/tabs/WeChatTab.test.tsx > WeChatTab QR login routing > URL-encodes the profile id in the admin route` | 3 | profile id 经 encodeURIComponent 形成单一 URL segment，防止路由段注入；不冒充 filesystem traversal。 |
| `src/components/tabs/WeChatTab.test.tsx > WeChatTab QR login routing > polls the same admin route with the session key` | 4 | QR start 后 poll 保持同一 admin profile，body 携带同一 session_key，不串会话。 |
| `src/components/tabs/WeChatTab.test.tsx > WeChatTab QR login routing > mirrors the server-pushed channel into local config on confirm` | 2 | confirmed 后合并 server-pushed channel，后续 Save 不被旧 channels 覆盖；mock 无 bot_token，不声称脱敏。 |
| `src/pages/profile/HomePage.test.tsx > HomePage sub-account gateway actions > shows a refusal as an error toast instead of "Gateway 'sub-1' started"` | 1 | Home 子账号 action 消费 `ok:false`，显示后端拒绝而非成功。 |
| `src/pages/UsersPage.test.tsx > UsersPage destructive confirmations > uses ConfirmDialog instead of native confirm for allowlist removal` | 1 | allowlist 删除经显式确认后才调用对应 API。 |
| `src/pages/UsersPage.test.tsx > UsersPage destructive confirmations > cancels allowlist removal without calling the API` | 4 | 取消 destructive dialog 不产生删除副作用。 |
| `src/pages/UsersPage.test.tsx > UsersPage destructive confirmations > uses ConfirmDialog instead of native confirm for account deletion` | 1 | account deletion 经确认后才调用 deleteUser。 |
| `src/pages/UsersPage.test.tsx > UsersPage bulk account management > selects all visible accounts and bulk deletes only that filtered set` | 3 | bulk delete 只作用于当前 filtered visible set，不误删隐藏账号。 |
| `src/pages/UsersPage.test.tsx > UsersPage bulk account management > reports per-account bulk delete failures and keeps failed accounts selected` | 4 | 部分删除失败逐项报告，失败账号保持选中以便恢复/重试。 |
| `src/pages/UsersPage.test.tsx > UsersPage role management > confirms and sends Promote role changes` | 3 | Promote 经确认后发送 admin role 变更。 |
| `src/pages/UsersPage.test.tsx > UsersPage role management > does not update a role when confirmation is declined` | 3 | 拒绝 role 确认时不调用权限变更 API。 |
| `src/pages/AdminAuditPage.test.tsx > AdminAuditPage > submits actor action and date filters` | 1 | actor/action/from/to 过滤条件实际进入 audit API 查询。 |

## 监控、默认继承与恢复

| 测试名 | 类别 | 守护性质 |
|---|---:|---|
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > persists a per-profile watchdog override without changing the system default` | 2 | per-profile override 持久化且不修改 global default。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > inherited rows and the system default > moves inherited rows with the system default and leaves overrides alone` | 2 | global 变更只影响 inherited rows，保留显式 override。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > inherited rows and the system default > keeps the saved state when a save fails, for the row and for the default` | 4 | save 失败回滚 optimistic row/default，不以未确认状态覆盖 server 状态。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > inherited rows and the system default > shows what the server kept when it did not keep the new default` | 4 | server 未接受新 default 时展示其真实返回值，不显示未生效选择。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > inherited rows and the system default > names its switches and sends one toggle while one is in flight` | 4 | in-flight 期间只发送一次 toggle，防止重复请求和标签混淆。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > inherited rows and the system default > keeps a row saved while the default changed on the current default` | 4 | override save 与 global default 变更并发时，以最新 default 重算 inherited effective state。 |
| `src/pages/ServerMetricsPage.test.tsx > ServerMetricsPage freshness indicator > shows last successful update age after a refresh failure` | 4 | 刷新失败保留 last-success 时间与错误，不把陈旧 metrics 当新鲜。 |

## 逐项核问

| 核问 | 清单回答 |
|---|---|
| 跨功能交互 | Auth create+`/me`、profile adapter、navigation→delete/save、QR page→tab→fetch、per-profile/global monitor 均有第 2 类链路。 |
| 失败/迟到路径 | rotation/create/action 失败、迟到 GET/save/action、monitor save failure、metrics refresh failure 均保留。 |
| 取消 | allowlist cancel 与 role decline 证明无 API 副作用；ProfileContext active/request generation 防旧响应。QR interval 有 cleanup 代码但无直接 unmount 测试，明确列为缺口。 |
| 崩溃恢复 | 无浏览器 crash/service-worker 恢复 harness；bootstrap re-rotation、navigation scope reset 只覆盖对应恢复链，不扩大声称。 |
| 数据完整性 | trim 一致、旧 config 清理、stale response 隔离、bulk filtered delete、partial failure selection、server-kept state 均验证。 |
| 并发/锁 | in-flight toggle、loadRequest generation、default/override 竞态覆盖；dashboard 无 Rust lock/thread，不能冒充服务端并发。 |
| 精确边界 | token 7/8/9、URL segment encode、role 确认/拒绝、bulk visible-set、stale response 边界保留；服务端 body cap 不在此层。 |
| 协议帧 | HTTP/JSON route/body 与 QR poll session 有覆盖；无 MCP/streaming frame codec。SSE log URL 仅是字符串构造，不是帧测试。 |
| 平台差异 | jsdom 浏览器行为有覆盖；无真实浏览器、OS process 或 sandbox 测试，不能声称跨 OS backend。 |
| peer/session | login/me、session token、scopedProfile/host scope 有前端链；dashboard 无 peer-to-peer session，server token/peer 授权不在此层。 |

## 已确认缺口（不以 mock 或弱断言冒充覆盖）

- admin token/OTP 的非 solo 登录、non-404 solo 失败、真实 server/off-loopback/proxy 授权、rotation hash/旧 token 失效没有 dashboard 集成测试；保留项只覆盖现有前端分支。
- `ProfileContext` 对非 admin sub-account 也选择 admin log URL 的代码路径没有测试；不声称 log route 已验证。
- context delete/purge 测试不覆盖 HomePage 在 context 返回后仍导航的行为；失败导航需另案，不在删除 PR 夹带生产修复。
- Sidebar 的 non-admin host-scope 测试只有 null scope；admin=true/false 导航共享 spy，不能区分 endpoint adapter 选择。
- SSE `getLogStreamUrl` / `getAdminLogStreamUrl` 将 bearer token 放入 query string，当前没有日志/历史脱敏测试；WeChat 固定行只证明 token 不落 config。
- QR interval 的 unmount/profile-change cleanup、真实网络 TypeError、非 ASCII token 长度和 1 MiB server body cap 均未覆盖。
- 所有 API 均为 mock；AdminBot `listAllProfiles` 还绕过 pagination，AdminAudit 绕过 queryPath 序列化，不能据此声称端到端服务端正确。

## 有意不保留的 29 项（仅在清单合入后删除）

| 测试名 | 审计理由 |
|---|---|
| `src/api.test.ts > ensureActionOk > passes an ok: true response through untouched` | 由 ProfileContext/Home 真实 action 成功链覆盖。 |
| `src/api.test.ts > ensureActionOk > throws the backend message on ok: false` | 由 solo/subaccount refusal 的实际 toast 覆盖。 |
| `src/api.test.ts > ensureActionOk > falls back to the caller message when the refusal carries none` | 纯 helper fallback；不为无消息边角另留单测。 |
| `src/components/BootstrapGate.test.tsx > BootstrapGate > does not redirect to /setup/welcome when token status returns 403` | 与 401 共用 `ApiError.isAuthError` 分支，保留 401 代表。 |
| `src/components/Sidebar.test.tsx > Sidebar admin-global gating > shows admin-global links when admin is NOT host-scoped` | 正向展示变体；保留 host-scope 与 non-admin 授权边界。 |
| `src/components/SoloProfileForm.test.tsx > SoloProfileForm > rejects a username containing spaces` | 与通用“字段有效前禁用”重复。 |
| `src/components/SoloProfileForm.test.tsx > SoloProfileForm > rejects an email with no @` | 与通用“字段有效前禁用”重复。 |
| `src/components/tabs/WeChatTab.test.tsx > WeChatTab QR login routing > uses the admin route for the profile being edited` | MessagingPage admin 链更强，覆盖 page→tab→fetch。 |
| `src/components/tabs/WeChatTab.test.tsx > WeChatTab QR login routing > uses the user-scoped route when viewing the own profile` | MessagingPage own 链更强，重复。 |
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > consumes ok: false on the sub-account adapter start too` | 与 subaccount restart/Home refusal 重复。 |
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > shows a failed self-service restart as an error toast instead of "Gateway restarted"` | 与 stop→start 两段 restart 的具体失败重复。 |
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > shows a failed self-service stop as an error toast instead of "Gateway stopped"` | 与同一 ok:false action 机制重复。 |
| `src/contexts/ProfileContext.test.tsx > ProfileContext gateway actions > still surfaces thrown request failures as error toasts` | thrown failure 与保留的 API refusal/failure recovery 属同一通用分支。 |
| `src/pages/AdminAuditPage.test.tsx > AdminAuditPage > renders audit entries from the admin audit API` | filter 测试已先等待 entry 渲染并验证实际查询。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > inherited rows and the system default > follows inherit → override → inherit, then the default, with the server row each time` | 顺序 UI 生命周期由 per-profile/global/save-failure/late-inherit 代表。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > lists admin-mode profiles and highlights the running admin bot` | 列表/highlight 展示，不是状态修改或恢复链。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > renders profile monitor override rows with inherited effective state` | effective-state 文案由实际 global/override 变更测试覆盖。 |
| `src/pages/AdminBotPage.test.tsx > AdminBotPage > shows an empty state when no admin-mode profile exists` | 空列表展示，不是用户链。 |
| `src/pages/LoginPage.test.tsx > LoginPage solo affordance > shows the solo button when local_solo_enabled is true` | 两个实际点击 solo-continue 的流程已隐含证明。 |
| `src/pages/ServerMetricsPage.test.tsx > ServerMetricsPage freshness indicator > shows how long ago metrics were successfully updated` | refresh-failure 测试同时验证成功时间戳与失败后的年龄。 |
| `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > disables submit when the entry is only whitespace` | trim 与精确长度边界覆盖，无独立用户分支。 |
| `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > does not allow submitting a 7-character token` | 7/8/9 测试的子集。 |
| `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > does not render the email-receipt section` | 删除旧 UI 的文案断言，不是 rotation 行为。 |
| `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > renders a password input (typed by operator) and no generate/copy buttons` | prop/删除按钮快照，非轮换链。 |
| `src/pages/SetupRotateToken.test.tsx > SetupRotateToken > toggles between password and text input when the show/hide button is clicked` | reveal/hide prop，不是 token 持久化或脱敏证明。 |
| `src/pages/UsersPage.test.tsx > UsersPage bulk account management > filters registered accounts by search text and role` | 纯筛选展示；保留筛选后 destructive scope 与 partial failure。 |
| `src/pages/UsersPage.test.tsx > UsersPage destructive confirmations > closes the account deletion dialog with Escape without calling the API` | 与 allowlist cancel 的无副作用分支重复。 |
| `src/pages/UsersPage.test.tsx > UsersPage role management > confirms and sends Demote role changes` | 与 Promote 共用 role 更新映射，保留 Promote 与拒绝代表。 |
| `src/pages/profile/HomePage.test.tsx > HomePage sub-account gateway actions > still toasts success when the sub-account start reports ok: true` | ProfileContext 成功刷新已覆盖同一 action 成功语义。 |

## 阶段 1 验证

- Node `v22.23.2`，`npm ci` 后 `npm test -- --reporter=verbose`：**17 files / 87 tests passed**。
- 真实 Vitest 清单：**58 keep + 29 delete = 87**；missing/extra/overlap 为空。
- `python3 wake/test-lines.py --json`：同口径基线 **2,441 / 85**；删除 PR 必须给删后值，不能用 87 与 85 直接作数量差。
- 本 PR 只新增本文件；required job 名、check-arm、通用 selector、生产代码和测试均未改。
