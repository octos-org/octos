---
name: pptx
description: "制作或修改可编辑 PPTX，尤其适用于沿用既有插画风格、生成全稿与无字底稿、叠加原生文字，以及评审演示文稿的视觉 UX。包含真实分辨率记录和美术验收；普通文案讨论不必启动生图或导出流程。"
---

# 可编辑 PPTX

制作模型负责内容、设计、看图与修复；独立评审会话负责复核实际交付稿。工具负责文件、字体计算、实际渲染、测量和证据整理；工具成功或检查项全绿不等于视觉通过。普通文案、解释或只读检查不自动启动整稿重做。

新环境首次制作时先按 [portable runtime](references/portable-runtime.md) 检查依赖，
调用随包的 `pptx_tool`（`action: doctor`）；具体接口见 [Rust runtime](references/rust-runtime.md)，技能目录来自当前 catalog。
不依赖已有评测目录，不把安装完成称为视觉验收通过。

随包流程使用 Rust `pptx_tool`，无需 Python 或 LibreOffice；这个约定也包括评审 JSON、批量命令和预览的辅助处理。页面与评审数据用现有读写/编辑工具处理，批量数据修订用 `patch`，预览用原生 `build`/`batch`/`render`；不要另写 Python 包装脚本再调用 Rust。若现有操作不足，明确记录具体缺口，不静默增加解释器依赖。历史回合已经使用过 Python 时如实报告；最终一步调用 Rust 不等于全流程未使用 Python。

## 制作与返修必须使用 peer，并落实退出标准

协调者自行选择符合约束的合并方式。评分口径有争议时交给独立仲裁 peer，工具能力不足时交给技术 peer；同一根因只调查一次，各页仍由原 reviewer 验收。旧裁定里的 `task_owner` 也按这两条路径接回。已验证所有授权修法都无法达到门槛时，记录阻塞证据并结束该次尝试，不申请人工豁免，不以重复评审代替修复。

独立技术报告一经发布，交回原 reviewer 在原裁定上绑定。报告字段或路径写错只修登记；正在测量的同一问题只等现有 peer 回报。所有剩余页面均已登记受阻时，协调者写明未交付页和原因，关闭本任务已消费的监视器，并将已有目标置为 blocked。相同证据不再触发制作、仲裁或全稿复审。

制作、重做或修复 PPTX 成品时，采用“协调主会话＋制作 peer＋独立评审 peer”，Rust 提供 QC 测量与证据校验；用户明确指定其他模式时以用户要求为准。启动前读 [peer 评审与退出](references/peer-review-exit.md)。Octos 必须实际调用 `peer_handoff`，保存真实会话回执并确认制作与评审双方能直接看图；不能以同一模型自评、普通后台脚本或仅写了分工说明冒充 peer。既有两会话运行保留其真实记录，不为改变角色名称中途重启。

整稿多页制作或重建，默认在共享设计约定冻结后采用**一页一个制作 peer**，同页返修继续交给原 owner，独立 reviewer 不制作页面。先列出全部页面、真实 session ID、独占路径和依赖；共享母版/样式只设一个 writer。名额有限时保留每页独立 owner、按宿主能力分批运行，不静默退回一个 peer 包多页；用户明确选择其他分工时从其要求。实际并发用运行区间证明，不能用分工表冒充。局部修改复用未改页的有效验收，不为此重开整稿。具体做法见 [independent pages](references/independent-pages.md)。

制作 owner 全覆盖不等于评审与返修已并行覆盖。协调者每轮先核对全部未完成页面及当前 peer 执行状态，把所有已就绪且相互独立的制作、评审、返修派出，再做长时间整稿审计或等待。有已授权、已连接的空闲独立 reviewer 时，不把其他就绪页面都排在同一忙碌 reviewer 后面；真实依赖须逐页写明，已验收页无需为填满名额重开。先读 `help topic:page_dispatch`，按当前 schema 用 `page_dispatch` 检查声明的派工覆盖，并结合 `page_gate` 的逐页 `work_stage` / `candidate_current` 整理未完成工作。它不派工、不打分、不授权，也不证明 peer 实际执行；这不是新的内核 EndTurn 门禁。

使用页面 JSON 与原生构建回执的整稿 page-first 流程，首次制作前按 `help topic:page_gate` 建立 `.octos/pptx-page-plan.json`，确认 `doctor.page_first_gate.supported` 与 `active`。每页由独立 reviewer 对**当前确切候选**打分，默认至少 95/100（用户明确要求优先）、零 critical、无未关闭必修项。全部页面通过后才合并；包括试验稿在内，不提前构建多页 PPTX。支持该能力的原生 `build`/`cycle`/`batch` 在写出多页文件前复核门禁。旧版本或没有源数据/构建回执的路线明确只有派工与验收约定，不伪造回执、不宣称已硬拦截。合并后仍由独立 reviewer 评审最终文件。

新任务先在实际 profile 上走通一页。确认底图来自该页获准的素材或生图作业，禁止脚本加工时直接绑定获准底图；核对画布与渲染尺寸、reviewer 实收像素、评审提交和外环接续。在计划中固定计分项、允许保留位图的范围及各页独立 reviewer。已确定的原始底图可用计划行的 `approved_background` 绑定；这验证字节来源，需要另有生图作业记录证明工具来源。没有获准素材时不能自行换路线。

宿主 `doctor.page_first_gate.review_submission` 为真时，优先用 `cycle(publish_candidate:true, freeze_inputs:true)` 发布单页，reviewer 用 `page_review_submit` 对该 `candidate_id` 一次提交。Maker 和 reviewer 分别使用产物的 `candidate_id`、`verdict_id` 作为原生 `peer_publish.handoff_id`（先确认宿主工具声明此参数）。相同交付重试复用该 ID，协调者按页派下一步，无需等其他页面。保留有效审核引用；报告格式更正和派工重试不新建 PPTX。下面的 [independent pages](references/independent-pages.md) 包含字段与旧版本兼容方式。

先约定必修项、可选微调、停滞窗口和用户已有预算。必修项关闭且整稿复核通过就交付；连续无实质改善就退出当前修法；真实阻塞或预算耗尽时交回未完成结果。偏好性微调不能无限重开返修。

等待 peer 时先用 `peer_list` 查当前回合状态，以 `peer_gather` 的 `wait_ms:0` 或不带等待时间读取已有结果；分别看当前执行状态和历史结果，旧结果的完成文字不能覆盖新一轮的运行/排队状态。已结束的回合不再等下一次黑板变化。先派出其他就绪独立工作，只有相关工作仍在运行时才使用有界等待；黑板变化和等待超时本身都不证明当前回合已完成。

交回当前轮等待自动唤醒前，按 [independent pages 的唤醒检查](references/independent-pages.md#verify-the-next-publication-can-wake-the-coordinator) 核对当前阶段实际会发布的位置、真实未完成任务和原生续跑链路。制作者候选、评审结果与 peer 结束事件是不同的触发源；只监视评审目录会漏掉制作完成。`peer_results_ready` 提示、monitor 的 `active`、已有结果文件都不能单独证明外环会自动继续。整批汇总可能等待其他 peer，须核对遗留排队输入与连接；仅由有权 owner 通过原生接口退役已确认无剩余工作的辅助 peer，不删除队列或重置历史。用一次真实交付后的外环接续、下一步派工记录验证修复。

区分回合结束与任务结束：制作或评审 peer 仍在运行、正常等待结果或按评审返修时，任务保持 `in_progress`。尚未达到 QA 标准本身不构成 `blocked`。只有具体输入、权限、配额或工具故障实际阻断后续工作，且没有可行的已授权路径时，才报告 `blocked` 并保留证据。

Octos 制作流程按 [continuation evidence](references/continuation-evidence.md) 初始化并核验现有退出策略。共享工作区只有一个整稿策略 owner，由协调者管理；页面 maker 不各自覆盖同一策略。独立工作区可各自管理自己的策略。**技能已安装、policy 文件存在、peer 已启动，都不证明内核已消费检查证据。** 首个完整修复检查周期后必须确认运行时状态与该证据一致；缺口要明确报告并处理，不能继续声称阈值已启用。保留已运行任务的历史，不静默改写冻结实验。

## 先确定设计路径

- **从文稿新建或重做图文丰富的稿件**：先读 [视觉设计与验收](references/visual-review.md)，把论点转成整页的图形、关系与标签布局，再进入构建。用户指定 image-first／海报式全页生图时，按 [成对设计](references/paired-workflow.md) 生成完整有字设计、从同一设计派生无字底稿，再重建可编辑文字。生成几个局部插画再塞入固定栏位，不等于完成整页设计。
- **已有被认可的设计图与无字底稿**：沿用其构图，进入下面的原生叠字流程；不用重复设计或生图。
- **用户明确要求图形、表格也全部可编辑**：选择适合的原生图形／图表实现，同时遵守整页视觉要求。单说“可编辑 PPTX”不自动要求每个像素都矢量化，也不授权改变指定的生图路径；说明实际可编辑范围。

用户要求改变信息密度、图文关系或版式时，重新检查整页构图与生图提示词；保持旧版位、仅换插图或缩字，不能证明已满足这个修改。两栏本身不是错误，是否适用由本页论点与用户要求决定。

## 保持本任务选定的流程

开始制作时，在项目已有的约定／制作记录中简短记下：内容来源与范围、选定路径、参考设计、可编辑范围、字体与分辨率要求。记录当前产物、尚未完成的步骤和真实阻塞；不另建一套调度或验收框架。开始、恢复任务或接到局部修改时核对这份记录。用户说“继续”、改字体或修一页时，原有范围与流程继续有效；用户明确改变要求时更新相关约定。

对已选定的 image-first 路径，有字整页图是布局参考；从它去字后叠原生文字，才能同时保留设计与可编辑性。不能以“图片里的字不能编辑”为由改成只生成无字插画、固定栏位排版或只交整页图片。已有合适的有字设计可直接复用。只需复用仍然有效的步骤；只有实际输入缺失、工具失败或不支持的能力才构成阻塞，排版困难本身不授权更换流程。

模型决定构图、提示词、标签位置和修复方式。实现可以调整，但须保持用户选定路径及其可核对的产物关系。分批完成时更新进度，继续处理已获授权的剩余工作；进度汇报不等于任务完成，不逐批重新请求“继续”的许可。

## 常用制作路径

1. 确定内容版本、页数、比例、字体和可编辑范围。最新文稿决定说什么，用户认可的参考决定如何画。固定的是内容与图文关系，不是旧稿的文本框数量、对象 ID 或一句话一个框的切法；在建立排版与验收约束前，先区分需要编辑的文字片段与底图已有的图形符号，保留可核对的内容映射。已记录的约束不能偷偷删除来绕过检查。实际查看参考图；可用时调用 `pptx_tool` 的 `ocr` 获取文本和四角候选，按原文、出现次数和周围图形核对，不能按识别顺序机械配字。OCR 辅助定位，不能替代原文。保留被指定的旧版。
2. 对“无字插画位图＋原生文字”，使用 [native builder](references/native-builder.md)，只编写页面数据。共享字体与构建实现复用一次。独立视觉对象的标签分别定位；不要把多个卡片的标签塞进一个靠空行对齐的文本框。
   遇到倾斜铭牌、斜向文字或透视面，按 [标签几何](references/label-geometry.md) 测量具体标签四角，重建局部文字框及旋转；检查字形与斜边、基线的关系。整个场景面板不能替代局部标签边界，旋转也不等于透视变形。
3. 先从底图确定局部放字区域、方向及同类字号，再用 `pptx_tool` 的 `fit` 定位字形；同类文字声明 `typography_groups` 并使用 `group` 固定共同字号，放不下时看具体冲突，不按每个 OCR 框分别放大或缩小。跨多个图形的文字可用 `split_text` 保留原文映射后拆框。接口见 [字体提案与修订](references/fit-and-revise.md)。实际渲染与测量共用 DeckCraft 字体引擎，不以测量代替看图。
   同组标签已有明确局部 owner 时，用 `layout_region` 一次比较几个共同字号的完整方案；工具联合检查行数、邻接文字和声明的障碍，返回实际 PPTX 预览。底图已有的箭头/运算符可在冻结契约前用 `represent_symbol` 保留原文映射，不能重复叠字或删除原文；文字和数字不适用。见 [语义与区域排版](references/semantic-authoring.md)。
4. 制作/返修阶段每个 maker 用带 `page` 选择器的单页 `cycle` 完成构建、重开、全稿/无字稿 PNG 和检查；全部页面独立验收通过后，优先用 `page_assemble` 按计划顺序合并已审核的实际单页 PPTX，再渲染做独立整稿审核。源数据能完整复现已验收页时，也可使用有序根 spec。批量修正数据用 `patch`，新候选用新目录。有参考设计的页必须声明 `design`；逐页完整分类差分标记与未标记区域，不只挑封面或已知缺陷。缺失、过期或未判定不能算通过。复用未改部分的有效证据，用 `crop` 看有上下文的局部；差分与零越界不能代替看图。无参考的原生页面不虚构参考。接口及旧稿补检见 [Rust runtime](references/rust-runtime.md)。
5. 按 [三项独立核对](references/review-evidence.md) 检查最终 PPTX：原稿与原生文字的完整意思、有字设计与无字底图的图形变化、实际渲染的阅读与标签归属。另按 [视觉验收](references/visual-review.md) 比较整页图文关系、信息密度和全稿节奏。全页检查不局限于检测器标记的对象；密集区域用清晰的上下文裁图。检查结果中的未知项保留为未知，复用未改变部分的有效证据。首个可读 cycle 完成文件/构建检查就发布交接，不先进行完整逐对象视觉自审；完整审查由独立 Reviewer 负责。制作 peer 返修后只自查改动及相邻对象，再发布新候选，按退出标准完成，不要求用户逐轮批准。

Maker 首次发布前只核对构建成功、文件存在与页数，不制作完整 review_pack、不填逐对象验收模板，也不等待“自己满意”。完整首轮视觉审查只由 Reviewer 执行；Maker 可直接查看必要的局部图来实施修复。首次可读的 `cycle` 就把返回值中的 `peer_review_handoff` 数据交给评审主会话；它不是工具名，不要搜索或调用同名工具，也不自动发送通知。Octos 制作 peer 用 `peer_publish` 发布中间成果，协调者用 `peer_gather` 读取并用 `peer_send_input` 派发评审，不能先自修几十轮。该字段绑定实际文件；评审与后续制作复用同一组缺陷 ID，参见 peer 退出规则。

实际生成位图时使用当前环境可用的 imagegen 工具与其 skill；普通文字修改无需重新生图。需要所有箭头、图标或表格也可编辑时，选原生形状/图表/矢量方法，并明确交付范围，不能把位图底稿称为全矢量。

## 按需要读取

新任务的 Rust 验收另查“还原一致性”：能读清不等于保留原图的关系。
原图没有、转换后新增的干扰不能解释为可选微调；正常斜字和原有纹理也不能
仅凭接触计数判错。按 [review lifecycle](references/review-lifecycle.md) 填写
绑定原图／底稿／实际渲染的事实，使用局部候选、独立仲裁和现有退出阈值。
工具强制证据和状态，不能保证模型观察必然正确。

| 情况 | 参考 |
|---|---|
| 制作或返修的 peer 分工、验收与停止 | [peer review and exit](references/peer-review-exit.md) |
| 新机器安装、发布包或本地渲染服务 | [portable runtime](references/portable-runtime.md) |
| 首次使用数据构建器、字体提案或自动修订 | [native builder](references/native-builder.md)、[fit and revise](references/fit-and-revise.md) |
| 用户授权并行制作；自己是页面 worker | 主代理：[independent pages](references/independent-pages.md)；worker：[page worker](references/page-worker.md) |
| 多页效率、重复修改、异步渲染 | [efficient production](references/efficient-production.md)、[checked jobs](references/checked-jobs.md) |
| 生图、去字、成对素材和分辨率记录 | [paired workflow](references/paired-workflow.md)、[pipeline pitfalls](references/pipeline-pitfalls.md) |
| 参考风格与图解质量 | [visual review](references/visual-review.md) |
| PPTX 打开失败或跨应用差异 | [PowerPoint compatibility](references/powerpoint-compatibility.md)、[PPTX-first audit](references/pptx-first-audit.md) |
| 错字、漏标签、重复值、归属或布局错误 | [reconstruction repair](references/reconstruction-repair.md)、[text-region audit](references/text-region-audit.md)、[标签级图差与间距](references/label-comparison.md)、[text reinspection](references/text-layout-reinspection.md) |
| 图标/连线上出现疑似 OCR 字符 | [OCR artifact sweep](references/ocr-artifact-sweep.md) |
| Octos peer 制作/返修：退出策略初始化、证据及生效核验 | [continuation evidence](references/continuation-evidence.md) |
| 需要可选结构检查或第二意见 | [host checks](references/host-checks.md) |

无需加载所有参考、为每个对象写长报告或重新编写测量框架。技能和脚本不得包含固定稿件的页码、文案、坐标或已知答案；页面数据可以包含当前任务实际确定的内容与坐标。

## 交付要求

使用新版 Rust 工具时，多页 `review_pack` 默认只返回逐页请求计划，不生成整稿大证据包。Reviewer 按计划生成各页包并 `review_submit`；协调者用 `review_merge` 合并同一 cycle 的独立证据，再 `evidence_record` / `evidence_status`。先用第一页验证完整证据交接，不能等整稿评分后才发现无法收尾。先用 `review_pack` 读整页关系，再按 `view_batches` 逐组读原图／底稿／实际 PPTX 的原像素局部图；不把整稿图片一次塞给模型，也不以 OCR 结论替代看图。同时检查工具返回的 `relation_signals`，对测得的倾角／偏移逐项说明缺陷、有意设计或未知；“有意设计”还须引用该信号 `intentional_options` 中的 `support_id`，无依据保留未知；不能用保留左起点解释原图居中关系丢失。用 `review_submit` 绑定验收记录，避免手抄哈希。按 [对象级视觉证据](references/object-visual-review.md) 为每个文字对象记录归属、越界、对齐、方向、字体五项简短观察。工具会把漏检对象、未知判断和过期版本留为未完成；同页发生修改后必须复核最终渲染。修前登记缺陷、预先冻结独立视觉验证路径和局部证据复用见 [review lifecycle](references/review-lifecycle.md)。判断由任务选定模型作出，不隐式改用其他视觉模型。

- 以实际交付 PPTX 为检查对象。调用 `pptx_tool` 的 `inspect` 做限定结构检查；DeckCraft 成功不能证明 PowerPoint 能打开。测试过哪些应用就报告哪些。
- 查看最终实际渲染，核对文字与原文、数字/公式/单位、重复次数、字体、对齐、卡片归属、遮挡和小字。标题统一要求来自用户/风格约定。缺少 owner region 或整张大面板作为 owner，均不能证明局部标签摆放正确。
- 全稿与无字稿从同一母版派生并共享图形层。记录源图、裁图和最终输出尺寸；插值放大称“4K 输出”，不能称原生 4K 生图。上下成对图仍需检查几何与裁切。
- 给出 PPTX、预览、可复现源数据及用户要求的成对图片。说明可编辑范围、字体回退、已做检查与剩余问题。不要以哈希一致、文本框数量或机械检查通过替代美术验收。

复用宿主已有 peer 与退出能力，不另写裁决模型、轮询守护进程或第二套调度器。停滞窗口不是总轮数上限；时间/费用预算是另一项约束，只采用用户或宿主明确给定的值。技能约定、工具证据与内核强制能力分开报告。

For repeated visual repair, use the batch and `review_view` interfaces in [Rust runtime](references/rust-runtime.md) to return source pixels plus measurements together. Keep independent object judgments and final rendered-PPTX review; batching does not approve quality.
