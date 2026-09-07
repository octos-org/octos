# 编译目录池与回收(build-cache pool)设计说明

状态:#2 设计产物。对应外环条目 .octos/OUTER_LOOP_REVIEW.md #1–#7。
本文引用的 file:line 均以 feat/build-cache-pool 分支(main 9c157101)实测代码为准;板面 #2 原先写的「clone/worktree 创建路径约 :2106、macos.rs :303 附近」经核对实际落点为 peers/mod.rs:1563–1770(clone 段 :1637–1652)与 macos.rs:303–378,本文按实际行号引用。

操作注记:本文件命中 .gitignore:68 的 `*.md`(git check-ignore 实测确认),默认被忽略且 untracked;提交需 `git add -f docs/build-cache-pool.md`,或仿 specs/*.md 的否定模式先例(.gitignore:75–77)加一行 `!docs/build-cache-pool.md`,否则「每条一个 commit」会卡在 add 这一步。

## 0. 背景与问题(为什么做)

2026-09-06 磁盘事故(#1 总纲):每个 peer 各自 `git clone`(peers/mod.rs:1637–1652)出一份独立工作区,每份工作区又各自长出一个 `target/`;cargo 不回收旧产物,也没有任何空间门。结果是 43 个 target 共 303 GB、herdr 两次因磁盘满关闭。

机制目标:octos 自身提供「编译目录池 + 分配 + 回收 + 空间门」。peer 与外环复验不再各养一个 `target/`,而是从每仓库受控数量的槽里领用;任务终态按状态释放;剩余空间不足时拒绝新的分配。

核心不变量(全文反复引用):

- I1 槽互斥:任一时刻一个槽至多一个持有者(文件锁为真值,元数据为参考)。
- I2 可复用不可误删:释放不删缓存内容;只有「无持有者 + last_used 超期」才可能被 GC 清空,且绝不用目录 mtime 做判断。
- I3 空间门先行:分配前先量剩余空间,不足即拒绝并给出可读错误,不 panic。
- I4 最小暴露:沙箱只放行本 peer 自己的槽,其他槽与其他仓库的池一律不可写。

## 1. 目录池布局与 repo-key

### 1.1 池根

```
<pool-root> = <profile data_dir>/build-cache/
```

`<profile data_dir>` 就是今天承载 `peers/` 的那个根:serve 侧 `runtime.data_dir.join("peers")`(api/ui_protocol_transport.rs:13212),默认 `~/.octos/profiles/<id>/data`(profiles.rs:164、resolve_data_dir profiles.rs:1894),profile 显式 `data_dir` 覆盖时跟随覆盖。

选它的理由:(a) peer staging 已经在这里,槽与 peer 元数据同根同生命周期;(b) serve 启动时会把 octos home 追加进 `read_allow_paths`(runtime/profile.rs:991–996),池在 home 内 ⇒ peer 沙箱默认可读,不需要为读再开口子;(c) `data_dir` 覆盖(profiles.rs:166)自动被尊重。

注意:`data_dir` 被覆盖到 home 之外时,上面的 (b) 不成立,此时槽的注入点必须同时授 `file-read*`(见 §7.3)。

### 1.2 repo-key

```
repo_key = hex(sha256(主仓库绝对路径的 canonical 形式))[0..12]   // 前 12 个 hex 字符
```

- 输入是「主仓库路径」,即 stage_peer 收到的 `workspace_root`(peers/mod.rs:1565)——master 的工作区根,**不是** peer 自己的 clone(`peers/<slug>/wt`,peers/mod.rs:1597–1599)。同一仓库派生的所有 peer 共用一个池。
- canonical 形式 = `std::fs::canonicalize` 后的 UTF-8 字符串(去尾部 `/`)。理由与 sandbox/mod.rs:220–226 对 HOME/CARGO_HOME 的 canonicalize 相同:符号链接(如 `/tmp` → `/private/tmp`)不应造出两个 key、两份池。
- sha2 与 hex 编码已是 octos-cli 依赖(crates/octos-cli/Cargo.toml:58),无新增依赖。
- 碰撞:48 bit,生日界约千万级仓库;即使碰撞,后果只是两个仓库共用池(缓存抖动),I1 的互斥锁仍然保证不会写坏,可接受。

### 1.3 槽目录

```
<pool-root>/<repo-key>/
  slot-N/            # peer 槽, N = 1..peer_slots(默认 2)
    .lock            # flock 锁文件(内容无意义,永不删除)
    holder.json      # 持有者元数据(仅在有主时存在)
    last_used        # 一行 unix 秒
    target/          # 真正交给 cargo 的 CARGO_TARGET_DIR 值
  verify-N/          # 外环槽, N = 1..verify_slots(默认 1),布局同 slot-N
```

要点:

- `CARGO_TARGET_DIR = <槽>/target`,控制文件(`.lock`/`holder.json`/`last_used`)与 cargo 产物分居,清空缓存就是 `remove_dir_all(target)` 不会误伤锁文件;`.lock` 的 inode 是互斥真值,绝不能被任何清理路径删除后重建。
- peer 槽与外环槽是**两个命名空间**:peer 永不领 `verify-N`,外环(`octos cache acquire --purpose verify`)永不领 `slot-N`。这样外环复验不会被在跑的 peer 挤占,peer 池大小也不因外环活动而缩水。
- `target/` 内放 `CACHEDIR.TAG`(与 cargo-gc.sh 的扫描约定一致,cargo 本身就会写)。
- 槽目录惰性创建:`acquire` 扫 `slot-1..slot-peer_slots`,目录不存在则建;`peer_slots` 只是上限,不是预分配。

## 2. 池大小与配置面

每仓库默认 2 个 peer 槽 + 1 个外环槽。可配置,配置面新增一个可选段(挂在 `Config`,config.rs:26,与既有可选段 `snapshots`/`tool_policy` 同构,config.rs:123/127):

```toml
[build_cache]
peer_slots   = 2      # 每仓库 peer 槽上限,>=1
verify_slots = 1      # 每仓库外环槽上限,>=1
min_free_gb  = 50     # 空间门阈值 GB,0 = 关门(仅诊断)
stale_hours  = 168    # GC:无持有者且 last_used 超过该小时数才可清(默认 7 天)
```

- `Option<BuildCacheConfig>` + `#[serde(default)]`,缺省即上表默认值;四项各自下界校验(peer_slots/verify_slots >= 1,阈值 >= 0)。
- `octos cache gc [--stale-hours N]` / `gate [--min-free-gb N]`(#5)只在命令级覆盖 `stale_hours` / `min_free_gb`,不改配置文件。
- 默认 stale = 168h 的依据:cargo-gc.sh v1 的候选报告就以「7 天内无新产物」为陈旧信号,7 天也保证工作日内的热缓存不被周维护误清。
- 不做全局「池总槽数」上限:每仓库上限 × 仓库数已是自然界;真要全局封顶留给后续条目,不在本线。

## 3. 分配协议(acquire / touch / release)

### 3.1 持有者元数据 holder.json

```json
{
  "kind": "peer" | "verify",
  "pid": <持有进程 pid>,
  "slug": "<peer slug,kind=peer 时>",
  "goal_id": "<可选>",
  "task_id": "<可选>",
  "purpose_note": "<verify 时为外环命令行>",
  "acquired_at": <unix 秒>
}
```

- goal/task 取自 stage_peer 已写入的 `peers/<slug>/goal`(peers/mod.rs:1726–1733 的两行格式),只作展示与诊断,不参与判定(判定只看锁 + pid)。
- 原子写:临时文件 + rename,直接复用 peer_io 的写法(peers/mod.rs:495 `write_peer_file_atomic` 的 O_EXCL 临时名 + renameat 模式)。

### 3.2 acquire(repo_key, purpose) -> Result<Slot, BuildCacheError>

1. **空间门**(I3):`fs2::available_space(pool_root)`(fs2 已是 octos-cli 依赖,Cargo.toml:59;fs2-0.4.3 底层即 statvfs/fstatvfs,`available_space` 见 fs2-0.4.3/src/lib.rs:180)。pool-root 不存在先 `create_dir_all` 再量。`available < min_free_gb * 1024^3` ⇒ 返回 `FreeSpaceLow { available_gb, min_gb }`;测量失败(无法 statvfs)⇒ 返回 `FreeSpaceUnknown`(fail-closed:这条线的全部意义就是防磁盘满,不因测量失败而放行;确需关闭门用 `min_free_gb = 0`)。
2. 依 purpose 选命名空间(peer → `slot-1..slot-peer_slots`,verify → `verify-1..verify-verify_slots`)。
3. 对每个候选槽:打开/创建 `<槽>/.lock`,`flock(EX | NB)`(fs2::FileExt,仓库既有用法见 autonomy/monitor_runtime.rs:410–437,MSRV 注释也照抄:std 的 flock 是 1.89+,保持 fs2 限定调用)。拿不到(EWOULDBLOCK)⇒ 下一个。
4. 拿到锁后:若 `target/` 不存在则建;写 `holder.json`;`last_used = now`;返回 `Slot { path, target_dir, lock_fd }`。**锁 fd 由持有方进程内存持有到 release**,这是 I1 的真值。
5. 全部槽被占 ⇒ `PoolExhausted { repo_key, kind }`,错误文案面向模型:「peer 池已满(2/2):每个 peer 的编译槽在其当前 turn 结束时释放,无需等 peer 关闭;稍后重试,或 `octos cache status` 查看持有者」。分配失败绝不等待、绝不排队(fail-fast,让 master 重新规划,而不是挂死一个 turn)。

### 3.3 touch(slot) 与 last_used 规则

`last_used` 只在两个时刻写:**acquire 成功时**和 **release 时**(取二者较晚)。语义 = 「该槽最近一次被持有的时刻」。

- 在持期间不需要周期刷新:持有中 GC 必然跳过(§6),刷了反而多 IO。
- 释放时刻写,保证「刚用完的缓存」从释放点起算陈旧窗口,而不是从 acquire 点起算。
- 明令禁止用 `target/` 目录或其内容的 mtime/atime 推断「还在用」:只读构建不落盘、`git clean`/restore 会重置 mtime、备份工具会全量刷 mtime——cargo-gc.sh v1 的头注就写着「target mtime is not a reliable unused signal」,这里把它固化成协议规则。

### 3.4 release(slot, outcome)

- `outcome ∈ { Completed, Failed, Cancelled, Retired }`,仅作记录(写进释放日志/事件),不影响行为。
- 动作:删 `holder.json` → `last_used = now` → drop 锁 fd(内核自动释放 flock)。
- **不删 `target/`**:释放即交还互斥权,内容留给下一个同仓库 peer 复用(I2,这正是本线的收益来源——2 个槽轮转即意味着 dep 编译产物被整个 fleet 复用)。
- release 必须可重入:slot 已无 holder.json 时是 no-op(§4.1 的 turn 终态主释放与 §4.2 的 close/evict 兜底可能先后各触发一次)。

### 3.5 崩溃后的启动期回收

进程死亡 ⇒ 内核释放 flock,但 `holder.json` 残留。回收(reclaim)规则,在 (a) serve/cli 启动期对全部池跑一遍,(b) `octos cache gc` 时跑:

1. 先 `flock(EX|NB)` 试 `.lock`:拿不到 ⇒ 有活持有者,跳过(锁是第一真值,元数据缺失也救得回来)。
2. 拿到锁后读 `holder.json`:不存在 ⇒ 无主,进入 §6 陈旧判定;存在则查 pid 存活——`kill(pid, 0)`:`ESRCH` = 死 ⇒ 删 holder.json(降级为无主,再走 §6);`EPERM` = 活着但不属于我们 ⇒ 视为持有,跳过;`0` = 本进程或同 uid 活进程 ⇒ 视为持有,跳过。
3. 残余风险(pid 复用把死持有者认成活)记录在案:holder.json 带 `acquired_at`,可人工核对;不做进程启动时间比对(macOS 无 /proc,Linux-only 方案不值当)。

## 4. 释放时机(与 peer 生命周期的对接)

**槽的生命周期 = peer 会话的「一个 turn」,不是 peer 会话本身。** 每个 peer turn 在自己的启动段 acquire,在该 turn 的终态 release;不要求 peer 会话 close。对应 #4。

为什么不能把主释放挂在 close 上:peer 会话是**持久会话**(#438)——close 回调 `build_peer_close_callback`(api/ui_protocol_transport.rs:15138)只由 originator 显式 `peer_close` 触发,而 peer wire 会话按 slug 注册、断线可重连(`register_peer_wire_session` :2822 起,:2830 的 #436 防复活闸只拦已写 `closed` 标记者),一个跑完 turn 的 peer 通常**永远不会被 close**。若 release 只挂 close 回调,该 peer 会握着 flock 直到 serve 退出 ⇒ peer_slots=2 时第三个 peer 的 turn 将一直 `PoolExhausted`(死锁,不是排队)。仓库里已有同一形态的教训:成果分支收集最初也只挂 close,未 close 的 peer 成果滞留在 clone 里,后来改为 per-turn 收集(`collect_peer_branch` 调用点 :14406 的注释记录了这次 live soak)。本设计从第一版就把释放对齐到 per-turn 终态。

### 4.1 主释放点:per-turn 终态写 result.md 处

serve 路径上每个 peer turn 都收口于 `write_peer_result_if_peer_session`(api/ui_protocol_transport.rs:14279):Completed 在 :34277 调用,Errored/RateLimited 在 :34383 调用;同函数内紧跟 per-turn 的 `collect_peer_branch`(:14406)——「成果已落 result.md、分支已 collect」正是该 turn 的终态语义。release 挂这里:

- turn Completed(:34277)⇒ `release(slot, Completed)`;
- turn Errored / RateLimited(:34383)⇒ `release(slot, Failed)`。

对应地 **acquire 也 per-turn**:serve 侧在该 turn 的 boot 段(ui_protocol_transport.rs:32616–32652,重建 agent、重水化 goal/originator 的同一处);首轮槽由 stage_peer 的 staging 段先行领用(失败回滚见下)。**boot 段每个 turn 都会跑,包括首轮**,故首轮不是再 acquire,而是认领(adopt):

- **adopt 规则(首轮槽交接)**:boot 段读 `peers/<slug>/build-cache`(stage_peer 已持久化的槽路径,§7.4)。若其命名的槽 `holder.json` 的 slug 与本 peer slug 一致 ⇒ **认领既有槽**:接手其锁 fd、改写 holder.json(仍写本 slug,acquired_at 刷新),不再新 acquire。只有「无记录槽」或「记录槽 holder.json 属他人」才走 §3.2 全新 acquire。理由:首轮槽的 flock 已由同进程(stage_peer)持有,flock 锁随 open file description 走,同进程第二个 fd 的 `EX|NB` 必然 EWOULDBLOCK ⇒ 裸 acquire 会抢走另一槽(2 槽池被同一 peer 双持);且 boot 段若覆写 build-cache 文件,staging 槽就以活 pid 之名漏到 serve 退出——两处事故 adopt 一并消除。

solo 侧 `run_chat_peer`(commands/chat.rs:828)是单 turn 驱动(`process_message(brief)` 一次、写完 result.md 即返回),其收尾/`Err` 分支(chat.rs:806 附近的 eprintln)本身就是该 turn 的终态:`release(slot, Completed/Failed)` 挂在返回前。同一 peer 的下一个 turn 重新 acquire,可能拿到不同槽——无妨:槽内容本就跨 peer 复用(I2),持有只是互斥权,不承诺粘性。

staging 失败回滚:首轮槽已在 stage_peer 内 acquire ⇒ `cleanup_staged_peer`(peers/mod.rs:1912–1924)同函数内 release(见 #4 的失败回滚注意)。

### 4.2 兜底释放(secondary,幂等)

以下路径只覆盖「turn 未到达 4.1 终态」的异常面(取消/中断/崩溃/清场),release 全部幂等(§3.4),与主释放先后触发也只生效一次:

| 事件 | 现有落点 | 动作 |
| --- | --- | --- |
| peer_close 取消 / 中断在飞 turn | close 回调 `build_peer_close_callback`(api/ui_protocol_transport.rs:15138;写 `closed` 标记 :15182、close 侧 `collect_peer_branch` :15192、取消注入队列 :15197–15198)与 turn 中断(:1729–1739) | `release(slot, Cancelled)`(幂等) |
| 中断终态路径(client 侧 turn/interrupt 命中 peer 会话时) | `try_emit_terminal`(api/ui_protocol_transport.rs:34717,发 `turn/error code=interrupted`;中断源枚举 InterruptOrigin :1729–1739 含 **Client** 与 PeerClose 两源)。注意:中断 turn **不经** :14279 主释放——该函数 doc comment(:14273–14277)明写 INTERRUPTED threads 0、never reaches this writer,与 :34383 的 error 臂是两条路 | 中断终态路径必须**同样调用幂等 release**(`release(slot, Cancelled)`)。否则一次 client 中断即漏一个槽:serve 存活 ⇒ §3.5 的 pid 判活跳过,无自愈,peer_slots=2 时累计两次即 PoolExhausted 到 serve 重启 |
| parked → retired(close 写 `closed` 后 peer 不再复活) | :2830 的 #436 防复活闸 | 同 close 路径,`release(slot, Retired)`(幂等,主释放已生效则 no-op) |
| 会话驱逐(FIX-06:连接断开即会话终) | `evict_session`(api/ui_protocol_transport.rs:36776) | 兜底 `release`(幂等);peer 重连后下一 turn 走 4.1 重新 acquire |
| 进程崩溃 | flock 自动释放 + §3.5 启动期回收 | 无即时动作 |

监督任务的退役(`retire_peer_supervised_task`,peers/mod.rs:264–275)不动槽:它只在 close 路径触发且 take-一次,槽释放以 4.1 的 turn 终态为准,避免双写。

外环槽(#6):`octos cache acquire --purpose verify --repo <path>` 打印槽路径与释放命令(`octos cache release --slot <path> --token <claim-token>`);外环规程(OLP_OUTER_BOOT)负责在复验结束时调用。外环侧脚本崩溃同样由 §3.5 兜底。

## 4.5 外环槽使用(#6 落地后的确切调用方式)

`octos cache acquire` 的输出是**稳定契约**,供 OLP_OUTER_BOOT 逐行解析(脚本解析器认前缀,不认列号):

```sh
# 1) acquire — stdout 恰好三行,顺序固定
out=$(octos cache acquire --purpose verify --repo "$REPO")
SLOT=$(printf '%s\n' "$out" | sed -n 's/^SLOT //p')      # <pool>/<repo-key>/verify-N
TARGET=$(printf '%s\n' "$out" | sed -n 's/^TARGET //p')  # <slot>/target
RELEASE=$(printf '%s\n' "$out" | sed -n 's/^RELEASE //p')# octos cache release --slot <slot> --token <claim-token>

# 2) 编译验证(空间门已在 acquire 内强制;incremental 由 env 关死)
export CARGO_TARGET_DIR="$TARGET"
export CARGO_INCREMENTAL=0
cargo build/test/clippy …          # 在外环 worktree 里跑,产物落进共享槽

# 3) release — 幂等,可无条件放在清理段(trap 里也安全)
$RELEASE                            # 必须保留 acquire 返回的 --token 参数
```

要点:

- **持有真值是 `holder.json` + pid 判活,不是 flock**(§3.5 第 2 臂)。acquire 是一次性 CLI:它在返回前**有意丢弃** flock fd,槽的「在持」状态从那一刻起完全由 holder.json(记录的 pid)承载。默认记录的 pid 是**调用方父进程**(`--pid` 可显式覆盖)——即外环驱动 shell,它的生命周期正是复验的生命周期。父进程死了 ⇒ §3.5 把槽降级为无主 ⇒ 按超期规则回收。
- **`--json` 变体**携带同名字段(`slot` / `target` / `release` / `pid`),供非 shell 驱动消费。
- **`--note <text>`** 落进 holder.json 的 `purpose_note`,`octos cache status` 会显示(哪条外环线在持有)。
- **release 语义**:每次 acquire 生成全新的 UUID claim token,写入 holder.json 并嵌入 RELEASE 行的 `--token` 参数。release 在锁内比对 token;旧 token 不匹配时返回 `claim mismatch; unchanged`,不改 holder.json 或 last_used。无 holder.json 则返回 `already released`,保持幂等;路径不在池根下、或目录里没有 `.lock` ⇒ 报错退出非零(不误删任意目录);槽正被活 flock 持有(peer 槽或在跑的 CLI)⇒ 拒绝并提示。target/ 内容永不删除(I2)。
- **`--purpose peer` 不存在**:peer 槽由 serve 内部领用,CLI 只开放 verify 命名空间(§1.3 的命名空间隔离在命令面上同样成立)。
- **verify 槽互相不挤占 peer 槽**:acquire 只扫 `verify-N`(`verify_slots`,默认 1)。
- octoscode 侧 OLP_OUTER_BOOT.md 的对应规程改写(把「每仓库一个长期 worktree + 自管槽锁」换成上述 acquire/parse/release 流程)由外环处理,本仓库只交付命令与本文档的契约描述。

## 5. 空间门

- **量什么**:池根所在文件系统的 `f_bavail`(普通用户可用),经 `fs2::available_space(&pool_root)`。不用 `df` 子进程——仓库里已有一个 df -Pk 版(api/admin.rs:3068–3082),但它挂在 api feature 的 sysinfo 依赖后(Cargo.toml:111/154),且子进程解析脆弱;fs2 无 feature 门槛、同 crates 既有用法(monitor_runtime.rs)。
- **何时量**:每次 `acquire` 入口(#4 的 peer staging 与 #6 的 verify acquire 同一入口);`octos cache gate [--min-free-gb N]` 单独可调(#5,不足退出非零,供外环/脚本作前置门,替代裸 cargo-gc.sh --gate 的场景)。
- **语义**:阈值比较用字节(`min_free_gb * 1024^3`),显示用 GB 两位小数;`min_free_gb = 0` 表示只报告不拦。
- **错误面**:typed `BuildCacheError::FreeSpaceLow { available_gb, min_gb }` / `FreeSpaceUnknown`。在 peer staging 路径上转成模型可读的 RpcError 文案(「剩余 X GB < 阈值 Y GB,已拒绝分配编译槽;先 `octos cache gc --apply` 或释放磁盘」),在 cache 命令上转成非零退出码。绝不 panic(I3)。

## 6. GC 策略

`reclaim_stale(policy)`,由 `octos cache gc [--stale-hours N] [--apply]` 驱动(#5;默认只报告,`--apply` 才动手),启动期回收(§3.5)与其共用扫描:

对每个 `<repo-key>/` 下每个槽:

1. `flock(EX|NB)` 拿 `.lock`:拿不到 ⇒ 活持有,跳过。
2. `holder.json` 存在 ⇒ §3.5 的 pid 存活检查;活 ⇒ 跳过;死 ⇒ 删 holder.json 后继续。
3. 无主 ⇒ 读 `last_used`(缺失按 0 处理,即最陈旧):`now - last_used <= stale_hours` ⇒ 跳过。
4. 超期 ⇒ `remove_dir_all(target)`(保留 `.lock`;`holder.json`/`last_used` 此时本就不存在或已删),报告一条「reclaimed <槽> <GB>」。

红线(直接抄进代码注释):

- **绝不用目录/文件 mtime 判断陈旧**(§3.3 的三条理由)。
- **绝不删除 `.lock`**,也绝不「删了重建」:锁 inode 一换,互斥真值就断了一拍。
- 持有者存活判定失败的默认方向是「跳过」(fail-safe:宁可漏清,不可误清在用槽)。
- GC 不跨仓库做排序/配额(「全清最老的仓库」之类):每槽独立判定,行为可预测、可解释;全局配额留后续条目。

## 7. 与沙箱的关系

### 7.1 需求

peer 的 cwd 是它自己的 clone `peers/<slug>/wt`(peers/mod.rs:1597–1622),而槽在 `<data_dir>/build-cache/...`,**在 cwd 之外**。macOS 沙箱 profile 是 `(deny default)` 起步(macos.rs:448–461),写权限只来自显式 allow:cwd 的 `(allow file-write* (subpath "<real_cwd>"))`(macos.rs:307)、`.git` 直写(macos.rs:358–369)、工具链授权(macos.rs:315–332)、只读模式下的外部 scratch(macos.rs:417–443)。所以必须为槽新增一条 allow,且**只给自己的槽**——其他槽、其他仓库的池保持默认拒绝(I4)。

### 7.2 注入点选型

候选 A:塞进 `ToolchainWriteGrants.subpaths`(sandbox/mod.rs:184–187,由 `toolchain_write_grants(allow_network)` 生成,:209–247;macos.rs:324–330 逐条发 `(allow file-write* (subpath ...))`)。
**不采用**,两个硬伤:(1) `configured_toolchain_grants`(sandbox/mod.rs:269–275)在 `allow_toolchains = false` 时整体返回空,槽授权会被顺带吞掉;(2) macos.rs:309–314 明确「#1976 fence 或只读 workspace 抑制 toolchain 授权」(deny-wins),槽会被一起抑制 ⇒ 被 fence 的 peer 完全无法编译。

候选 B(采用):仿照「只读/受 fence 模式下外部 scratch 的独立授权」(macos.rs:417–443:不论 workspace 是否可写都独立发一条 subpath allow,并做 SBPL 元字符校验与 fail-closed 跳过)。具体:

- `SandboxConfig`(octos-agent,sandbox/mod.rs:37)新增字段 `build_cache_slot: Option<PathBuf>`(serde default None,默认行为零变化)。
- `build_backend`(sandbox/mod.rs:1075–1085)把它 canonicalize 后传进 `MacosSandbox`(模式同 `repo_git_write`,macos.rs:358–361)。
- macos.rs 在拼 profile 时独立于 workspace 三臂(macos.rs:333–378)发:
  `(allow file-write* (subpath "<real_slot>/target"))` + `(allow file-read* (subpath "<real_slot>"))`
  写权限只覆盖 `target/`；`.lock`、`holder.json`、`last_used` 不在该授权范围内。
  ——路径先 canonicalize、再过 `path_has_sbpl_metachars` 校验(与 macos.rs:318/325 同款),不安全即跳过该条(fail-closed:编译失败可见,profile 不可注入)。**独立于 fence/toolchain 开关**的理由:槽在 workspace 之外,是 harness 分配的基础设施目录,与 macos.rs 外部 tmp 的处理完全同构;fence 约束的是「workspace 内写什么」,不该顺带杀掉 workspace 外的编译产物目录。
- Docker 在每次 shell 调用时仅挂载本槽 `target/` 到容器内 `/octos-build-cache/target`，并通过 `--env` 传入该容器路径的 `CARGO_TARGET_DIR` 与 `CARGO_INCREMENTAL=0`。目标目录不可用或无法表示为挂载参数时拒绝执行。bwrap 与 Landlock 仍未提供池目标目录的可写授权，构建可能失败；NoSandbox 本就不拦。
- 单元测试面(#4 要求的「白名单仅含自己的槽」,不必真跑 seatbelt):直接断言生成的 profile 字符串含且仅含本槽 subpath(模式同 macos.rs 既有 tests,:498 起)。

### 7.3 读侧

默认路径下池在 octos home 内,serve 启动已把 home 加进 `read_allow_paths`(runtime/profile.rs:991–996),读已覆盖;但 `data_dir` 被覆盖到 home 外时 read_allow_paths 非空且不含池 ⇒ 这就是 §7.2 里 slot 授权同时发 `file-read*` 与 `file-write*` 的原因。

### 7.4 环境变量注入(CARGO_TARGET_DIR 如何到达 cargo)

关键事实:**serve 路径上 peer 与 master 同进程**(peer 会话就是同一 serve 里的一个 `peer-<slug>` topic 会话,run_chat_peer 的进程内驱动 commands/chat.rs:797–812 与 serve 的 per-turn agent 重建 api/ui_protocol_transport.rs:32616–32652)。因此**不能**用进程环境变量区分 peer —— `std::env::set_var` 会污染所有会话。注入必须 per-tool-call:

- 持久层:`peers/<slug>/` 下新增一行文件 `build-cache`(内容 = 槽路径),与 `goal`/`originator` 同模式原子写,由该 peer 当次持槽的 acquire 方写入——首轮是 `stage_peer` 的 staging 段,后续 turn 是 §4.1 的 per-turn boot 段(serve 侧 ui_protocol_transport.rs:32616–32652 读 goal/originator 的同一处,solo 侧 `read_peer_boot`,peers/host.rs:96–127,PeerBoot 加一字段)。文件只是「当前槽路径」的读回通道,随 per-turn acquire 覆写;真值仍是 §3 的锁与 holder.json。首轮例外:boot 段不覆写,而是按 §4.1 的 adopt 规则读回并认领 stage_peer 记录的槽。
- 传递层:`ToolContext` 新增 `build_cache_slot: Option<PathBuf>`,模式照抄 `goal_id`/`task_id`/`originator_session`(octos-agent tools/mod.rs:341–348,由 Agent 携带、execution.rs:508–511 逐调用下发)。变量名落在 `OCTOS_*` 命名空间还有一个好处:即便未来走到插件/子进程的 strict 清洗,`OCTOS_*` 也被保留(subprocess_env.rs:90–94 与 :113–127)。
- 应用层:shell 工具在 `wrap_command` 之后、与 `apply_frontend_tool_env`/`apply_git_tool_env` 同一位置(tools/shell.rs:201–204 前台、:1104–1107 后台)发 `CARGO_TARGET_DIR=<slot>/target` 与 `CARGO_INCREMENTAL=0`。`sanitize_command_env`(shell.rs:204/:1107,策略 subprocess_env.rs:159–171)只剥 secret/注入类变量,不会剥 `CARGO_TARGET_DIR`,顺序安全。
- `CARGO_INCREMENTAL=0` 即使机器级 `~/.cargo/config.toml` 已设也照发:peer 环境的 CARGO_HOME 不可控(#1 事故里 incremental 一项就占 31 GB),per-command 显式覆盖最稳。

## 8. 模块落点决策

**结论:池核心放 `crates/octos-cli/src/build_cache/`(新模块,`mod pool` 为核心;#5 的命令面放 `crates/octos-cli/src/commands/cache.rs`);octos-agent 侧只加「一个配置字段 + 一条 SBPL 规则 + 一个 ToolContext 字段」,不落任何池逻辑。**

依据(按实际分层,非偏好):

1. **谁是写入方**:池的全部状态改写都发生在 octos-cli——peer staging(peers/mod.rs 的 stage_peer 及其 serve 调用 ui_protocol_transport.rs:13284)、turn 终态释放(write_peer_result_if_peer_session :14279,调用点 :34277/:34383;solo 侧 run_chat_peer chat.rs:828)、兜底释放(close 回调 :15138、会话驱逐 :36776)、`octos cache status|gc|gate|acquire|release` 命令(#5/#6,commands/ 下)。外环接入(#6)更是天生 CLI。
2. **依赖方向**:octos-cli 依赖 octos-agent(Cargo.toml:24),反向不存在。池若放 octos-agent,octos-cli 的一切调用都要穿过一层与 agent 语义无关的 API;而 agent 侧真实需要的只有「我的槽路径」这一个数据(进 SandboxConfig 与 ToolContext),不是池逻辑。
3. **依赖就近**:fs2(Cargo.toml:59,锁 + statvfs)、sha2(:58,repo-key)都已在 octos-cli;放 octos-agent 得新加依赖。
4. **共享代码现状**:cli 与 agent 今天共享的东西(锁习惯用法 monitor_runtime.rs:410–437、原子写 peer_io、沙箱构造 create_sandbox sandbox/mod.rs:1005)全部是「cli 调 agent / cli 自持」形态,没有「agent 反过来调 cli 设施」的先例;本设计维持该方向。
5. **octos-agent 的改动清单因此收敛为三件小事**:`SandboxConfig.build_cache_slot`(sandbox/mod.rs:37 一带)、macos.rs 的独立授权规则(§7.2)、`ToolContext.build_cache_slot`(tools/mod.rs:261 一带)。这三件都属于「配置/上下文面」,正是两 crate 今天的合法边界,不引入反向依赖。

错误类型 `BuildCacheError`(thiserror)与 `Slot` 句柄同放 `build_cache/pool.rs`;repo-key 摘要函数放 `build_cache/repo_key.rs`(纯函数,便于单测);`status/gc/gate/acquire/release` 的命令封装放 commands/cache.rs,thin,只做参数解析与打印(输出必须脚本可解析,#6 的验收)。

## 9. 与后续条目的映射

- #3 池核心:§1/§3/§5/§6 的 `acquire/release/touch/reclaim_stale` + `BuildCacheError`;测试 = 并发 acquire 不超池大小、释放后可复用、持有者死亡可回收、空间门返回错误不 panic(临时目录池)。
- #4 peer 接入:§4.1 主释放(turn 终态 :14279/:34277/:34383 + per-turn acquire)+ §4.2 四条兜底 + §7.4 的 env 注入 + §7.2 的沙箱授权;测试 = peer 环境含正确变量、两 peer 拿不同槽、turn 终态释放(不依赖 close)、close/evict 兜底幂等、白名单仅含自己的槽(单测 profile 字符串)。
- #5 命令:`octos cache status|gc|gate`,阈值默认 §2;测试 = 命令级单测(临时目录池)。
- #6 外环:§4 末段的 acquire/release 命令对,输出可被 shell 解析。
- #7 收尾:全量 test/clippy/fmt,本文件随 #2 提交后不再回改(修订走新条目)。

## 10. 锚点速查(实测行号,feat/build-cache-pool @ 9c157101)

- peer staging 核心:crates/octos-cli/src/peers/mod.rs:1563(stage_peer 签名);:1592–1595 slug 预留;:1597–1599 cwd=peers/<slug>/wt;:1615–1628 为何是 clone 不是 worktree;:1637–1652 `git clone --quiet --no-hardlinks`;:1654–1668 建分支;:1726–1733 goal 文件两行格式;:1738 brief.md;:1760–1767 StagedPeer 返回。
- peer 终态/清理:peers/mod.rs:264–275 retire_peer_supervised_task;:1912–1924 cleanup_staged_peer;api/ui_protocol_transport.rs:14279 write_peer_result_if_peer_session(turn 终态主释放点,Completed 调用 :34277、Errored/RateLimited 调用 :34383);:14406 per-turn collect_peer_branch;:15138 build_peer_close_callback(兜底);:15177–15186 closed 标记;:15192 close 侧 collect_peer_branch;:15197–15198 取消注入;:1729–1739 peer_close 中断 turn;:2830 防复活闸(#438 持久会话);:36776 evict_session(FIX-06,兜底);:32616–32652 serve 侧 peer boot(goal/originator 重水化,per-turn acquire 段)。
- 沙箱:crates/octos-agent/src/sandbox/mod.rs:37 SandboxConfig;:149 allow_toolchains;:92 repo_git_write;:183–187 ToolchainWriteGrants;:209–247 toolchain_write_grants;:254–265 push_cargo_grants;:269–275 configured_toolchain_grants(deny-wins 抑制点);:1005 create_sandbox;:1075–1085 build_backend(MacosSandbox 装配);sandbox/macos.rs:307 cwd 写规则;:315–332 toolchain 授权发射;:333–378 workspace 三臂;:358–369 repo_git_write 规则(canonicalize 模式);:417–443 外部 scratch 独立授权(本设计 §7.2 的同构范本);:448–461 profile 拼装与 `(deny default)`;:464–469 TMPDIR 注入;:498 起 tests(规则字符串断言范本)。
- 环境注入:crates/octos-cli/src/commands/chat.rs:797–812 进程内 peer 驱动;:828 run_chat_peer;:854 create_sandbox;:875–882 Agent 装配;:886 goal 重水化。crates/octos-agent/src/tools/shell.rs:194 wrap_command;:201–204 与 :1104–1107 环境注入点;:327–332 apply_frontend_tool_env(范本);:754 apply_git_tool_env;:449–462 MAIN_TREE_SOVEREIGNTY_PROVIDER(host 装配范本)。crates/octos-agent/src/subprocess_env.rs:159–171 should_forward_env_name;:90–94 + :113–127 strict 变体保留 OCTOS_*。crates/octos-agent/src/tools/mod.rs:261 ToolContext;:341–348 goal/task/originator 字段(§7.4 传递层范本);agent/execution.rs:508–511 逐调用下发。
- 路径与数据根:api/ui_protocol_transport.rs:13212 `runtime.data_dir.join("peers")`;:11525 同;crates/octos-cli/src/profiles.rs:164–166 data_dir 覆盖;:1894 resolve_data_dir;:1939 octos_home_dir;crates/octos-cli/src/config.rs:26 Config;:123/127 既有可选段形态;runtime/profile.rs:991–996 read_allow_paths 追加 home。
- 空间与锁依赖:crates/octos-cli/Cargo.toml:59 fs2;:58 sha2;:24 octos-agent;:111/154 sysinfo(api-only);autonomy/monitor_runtime.rs:410–437 fs2::FileExt 用法与 MSRV 注释;api/admin.rs:3068–3082 df -Pk 版(不采用的对照);fs2-0.4.3/src/lib.rs:180 available_space(statvfs)。


### #7 与上游 #2236 的语义合并

`PeerBuildCache::Shared` 表示已从本池持有槽；不再生成指向 workspace 根 `target/` 的 Cargo 配置。每次工具调用仍通过 `CARGO_TARGET_DIR=<slot>/target`、`CARGO_INCREMENTAL=0` 使用该槽，沙箱只放行自己持有的槽。`RepoConfig` 保留仓库的 `.cargo/config.toml` 或旧式 `.cargo/config`，不注入池槽环境变量；未围栏、非 Cargo 或未注册池的根为 `None`。

staging 与每轮 boot 均检查上述条件；首轮接手 registry 中的原句柄，后续从 clone 的 `remote.origin.url` 找回源仓库，避免按 clone 路径分裂池。配置切换后不再适用的 staged 槽立即释放。池耗尽、空间门或记录失败会报错，不退回池外 target；记录失败同时释放新槽。终态错误（包括 agent dispatch 前的 peer lifetime 持久化失败）与中断都幂等释放槽。

serve 对省略的 `[build_cache]` 注册默认配置；side-table 未注册才表示该根不使用池，当前没有单独的 disabled 开关。`peer_staged` 的 shared / repo-config / 无后缀三态与 UI 协议保持兼容，Shared 的 note 改为准确说明槽池与环境注入。


#7 复审补齐了实际 shell 沙箱接线：foreground/background 每次调用只解析一次 `ToolContext` / task-local 槽，向环境注入和 contextual sandbox wrapper 传同一个值。macOS wrapper 复制共享 backend 的配置后替换槽（`None` 也覆盖旧值），因此不会把上一轮或兄弟 peer 的槽留在共享对象中；直接使用旧 `wrap_command` 的调用者仍保留显式配置语义。原有 SBPL 路径校验与写入围栏继续生效，其他 backend 保持既有行为（此处未补非 macOS 的池外目录挂载）。真实 ShellTool 前台/后台回归检查了 slot1→slot2→None 的 SBPL/env 一致性；macOS kernel 回归验证自己的槽可写、兄弟槽及随后撤销的旧槽拒绝写入。
