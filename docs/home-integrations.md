# 家庭场景功能是怎么工作的（Home Assistant / NAS）

> 用大白话讲清楚：新加的「智能家居」和「NAS（家庭存储）」功能是什么、怎么用、背后大致怎么运转。
> 前面几节不需要懂编程也能看懂；最后一节「给开发者」才有技术细节，不想看可以跳过。

## 一句话

以前你在聊天里只能跟 AI **聊天**，现在 AI 多了几只「手」：它能去**看你家设备的状态、开关灯**，
也能**翻你 NAS 上的文件夹、读文件**。你照常说人话，AI 自己决定该动哪只手。

没有单独的"智能家居 App 页面"——这些都长在你**现在用的聊天界面**里。

## 一个比喻

把 AI 想成一个**会听人话的管家**。管家本人不会拧螺丝，但他手下有几个**专项工人**：

- 一个只会跟你家智能家居系统打交道的工人（我们叫它 **ha_bridge**）
- 一个只会跟你家 NAS 打交道的工人（**nas_bridge**）

你跟管家说「把客厅灯打开」，管家听懂了，**派 ha_bridge 去开灯**，工人回来汇报「开好了」，
管家再用人话告诉你。整个过程你只跟管家（聊天框）打交道。

```
   你：「把客厅灯打开」
         │
         ▼
   AI 管家  ──派活──▶  ha_bridge 工人  ──▶  你家 Home Assistant  →  灯亮了
         │                                                          │
         └──────────  工人汇报「已打开」  ◀───────────────────────┘
         │
         ▼
   聊天里：「✅ 客厅灯已打开」
```

## 你现在能让它做什么

**智能家居（Home Assistant）**——说人话就行，比如：
- 「列出我家所有设备的状态」 → 出一张分房间的设备清单（灯、空调、传感器、门磁…）
- 「把客厅灯打开」「把空调调到 26 度」「卧室灯关了吗」 → 查询或控制

**NAS（家庭存储）**——比如：
- 「列出 NAS 上 /photos 文件夹里有哪些照片」
- 「读一下 /documents 里那份季度报告，帮我总结」
- 「在 NAS 上搜一下叫 budget 的文件」

> 进阶玩法：让它列出某个相册的照片 → 再交给已有的 **Slides 技能**做成相册幻灯片。
> （NAS 这块本次只做了"读"，做相册用的是之前就有的能力。）

## 怎么接上你真实的家（最实用的一节）

演示时用的是**假数据**（两个模拟服务器假装成你家 HA 和 NAS）。换成真实的，**一行代码都不用改**，
只要把几个"地址和密码"填对，然后重启后端就行。

填在这个文件里：`~/.octos/profiles/owner.json`（你的本地配置，不会上传）。

**智能家居**，在 `env_vars` 里加两项：
```jsonc
"HA_URL":   "http://你家HA的地址:8123",
"HA_TOKEN": "你的长期访问令牌"
```
> 令牌哪来：打开 Home Assistant → 左下角点你的头像 → 资料页拉到最底 → 「长期访问令牌」→ 创建 → 复制。

**NAS（群晖）**，加三项：
```jsonc
"NAS_URL":  "https://你家NAS的地址:5001",
"NAS_USER": "用户名",
"NAS_PASS": "密码"
```
> 安全建议：给 NAS 单独建一个**只读账号**给它用。证书是自签的话再加一句 `"NAS_VERIFY_TLS": "false"`。

填完**重启后端**（`octos serve`）就生效了。不想动配置文件的话，告诉我地址和令牌，我帮你填。

## 三种接法，简单说

接外部东西进来，有三条路，本次都铺好了：

| 接法 | 通俗讲 | 适合 |
|---|---|---|
| **HA 技能** | 我们自己写的"专项工人"，直接调 HA | 现成、稳，今天就能用 |
| **NAS 技能** | 同上，调 NAS | 同上 |
| **MCP** | 一个行业通用插头，能插现成的第三方"设备" | 以后接新东西不用每个都自己写 |

第三种（MCP）是顺手补的一个**底层能力**：以前网页版插不了 MCP 这种通用插头，现在能了。
好处是——Home Assistant 官方就出了一个 MCP 插头，以后想接别的（各种智能家居、笔记软件…）
很多都有现成 MCP，插上即用，不用再一个个手写工人。

---

## 给开发者：技术细节（不想看可跳过）

### 「技能」到底是什么

一个技能 = `crates/app-skills/<名字>/` 下的一个**独立小程序**（Rust 二进制），目录里有：
- `manifest.json`：声明这个技能有哪些工具、每个工具要哪些参数、要用哪些环境变量
- `SKILL.md`：给 AI 看的说明书（带触发关键词）
- `src/main.rs`：真正干活的代码

调用约定极简、语言无关：
```
./ha_bridge <工具名>     第一个参数是工具名
  stdin  ：一个 JSON（入参）
  stdout ：一个 JSON  {"success": true, "output": "文本", "files_to_send": [...]?}
```
AI 要用某工具时，octos 就 `spawn` 出这个小程序、把参数喂给 stdin、读它 stdout 的结果。
因为是**独立进程**，崩了也不影响主程序。

被发现的三步：① 在 `bundled_app_skills.rs` 注册（manifest 编进主程序）；
② `octos serve` 启动时把技能二进制拷到 `~/.octos/bundled-app-skills/<名>/main`；
③ 每个 profile 启动时扫描这些目录、把工具注册给 AI。
**本地从源码跑必须先 `cargo build -p <技能包名>`**，否则二进制不存在、`plugin_count=0`、AI 没工具。

### 密钥怎么安全地到达技能：两道白名单（最容易踩的坑）

密钥放在 profile 的 `config.env_vars`，但**默认不会**传给技能（防泄密）。要让某个 env 到达技能，
必须**同时**过两关：
1. **`FIRST_PARTY_SKILL_ENV_VARS`**（`profile_factory.rs`）：硬编码白名单，名字在册才放行。
2. **manifest 里每个工具的 `env` 声明**：技能还得自己声明"我这个工具要用 HA_TOKEN"。

两关都过，`std::env::var("HA_TOKEN")` 才读得到。`HA_*` / `NAS_*` 本次都加好了；以后做任何带密钥的
技能都要走这两步。

### MCP-per-profile：为什么以前网页版连不上 MCP

`ProfileRuntime` 本来支持启动 MCP server，但多租户 `serve` 路径下，`config_from_profile()`
把 `mcp_servers` 硬编码清空了，而且 `ProfileConfig` 没有 mcp 字段。本次修复（都在 `profiles.rs`）：
给 `ProfileConfig` 加 `mcp_servers` 字段（serde 默认、向后兼容）、`config_from_profile` 改为透传、
patch/diff 同步接上（改 MCP 触发重启，因为它只在启动时拉起）。

接 HA 官方 MCP 就只是在 profile 加一段：
```jsonc
"mcp_servers": [
  { "url": "http://你家HA:8123/api/mcp",
    "headers": { "Authorization": "Bearer <HA 长期令牌>" } }
]
```

### 本次改动文件

```
新增  crates/app-skills/home-assistant/   ha_bridge 技能（+15 测试）
新增  crates/app-skills/nas/              nas_bridge 技能（+26 测试）
改    crates/octos-cli/src/profiles.rs    MCP-per-profile（字段+透传+patch/diff+测试）
改    crates/octos-cli/src/commands/gateway/profile_factory.rs   env 白名单加 HA_/NAS_
改    crates/octos-agent/src/bundled_app_skills.rs   注册两个技能
改    Cargo.toml / Cargo.lock              workspace 成员
```
