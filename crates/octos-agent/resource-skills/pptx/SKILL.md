---
name: pptx
description: "制作或修改可编辑 PPTX，尤其适用于沿用既有插画风格、生成全稿与无字底稿、叠加原生文字，以及评审演示文稿的视觉 UX。包含真实分辨率记录和美术验收；普通文案讨论不必启动生图或导出流程。"
---

# 可编辑 PPTX

制作模型负责内容、设计、看图与修复；独立评审会话负责复核实际交付稿。工具负责文件、字体计算、实际渲染、测量和证据整理；工具成功或检查项全绿不等于视觉通过。普通文案、解释或只读检查不自动启动整稿重做。

新环境首次制作时先按 [portable runtime](references/portable-runtime.md) 检查依赖，
用随包的 `scripts/setup_runtime.py` 准备工作区环境；技能目录来自当前 catalog。
不依赖已有评测目录，不把安装完成称为视觉验收通过。

## 制作与返修必须使用 peer，并落实退出标准

制作、重做或修复 PPTX 成品时，采用“独立评审主会话＋制作 peer”；用户明确指定其他模式时以用户要求为准。启动前读 [peer 评审与退出](references/peer-review-exit.md)。Octos 必须实际调用 `peer_handoff`，保存真实会话回执并确认双方能直接看图；不能以同一模型自评、普通后台脚本或仅写了分工说明冒充 peer。

制作 peer 独占改稿；评审者查看实际 PPTX 渲染，按证据把缺陷交回同一个 peer。先约定必修项、可选微调、停滞窗口和用户已有预算。必修项关闭且整稿复核通过就交付；连续无实质改善就退出当前修法；真实阻塞或预算耗尽时交回未完成结果。偏好性微调不能无限重开返修。

Octos 制作 peer 同时按 [continuation evidence](references/continuation-evidence.md) 初始化并核验现有退出策略。**技能已安装、policy 文件存在、peer 已启动，都不证明内核已消费检查证据。** 首个完整修复检查周期后必须确认运行时状态与该证据一致；缺口要明确报告并处理，不能继续声称阈值已启用。保留已运行任务的历史，不静默改写冻结实验。

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

1. 确定内容版本、页数、比例、字体和可编辑范围。最新文稿决定说什么，用户认可的参考决定如何画。实际查看参考图；OCR 辅助定位，不能替代原文。保留被指定的旧版。
2. 对“无字插画位图＋原生文字”，使用 [native builder](references/native-builder.md)，只编写页面数据。共享字体与构建实现复用一次。独立视觉对象的标签分别定位；不要把多个卡片的标签塞进一个靠空行对齐的文本框。
   遇到倾斜铭牌、斜向文字或透视面，按 [标签几何](references/label-geometry.md) 测量具体标签四角，重建局部文字框及旋转；检查字形与斜边、基线的关系。整个场景面板不能替代局部标签边界，旋转也不等于透视变形。
3. 字体、换行或可见字形定位需要计算时，用 [字体提案与修订](references/fit-and-revise.md) 中的 `native_fit.py`。明确实际字体文件；`--ink-align` 可按字形边界和基线提议文本框位置。提案不改原稿，未明确允许缩小时不自动缩字；布局归属由模型根据图形判断。
4. `native_cycle.py run --inspect` 合并构建、全稿/无字稿渲染和测量，返回对象级问题、局部图和归属叠图。按 [共同页面方法](references/page-method.md) 批量修复；`native_cycle.py revise` 返回前后对照及仍存在/新增/不再检出的问题，`--resume` 继续同一个异步任务。未改动对象的问题仍需判断；边界变化不等于文字修好了。
5. 按 [三项独立核对](references/review-evidence.md) 检查最终 PPTX：原稿与原生文字的完整意思、有字设计与无字底图的图形变化、实际渲染的阅读与标签归属。另按 [视觉验收](references/visual-review.md) 比较整页图文关系、信息密度和全稿节奏。全页检查不局限于检测器标记的对象；密集区域用清晰的上下文裁图。检查结果中的未知项保留为未知，复用未改变部分的有效证据。制作 peer 自查后交回独立评审会话，按退出标准完成，不要求用户逐轮批准。

实际生成位图时使用当前环境可用的 imagegen 工具与其 skill；普通文字修改无需重新生图。需要所有箭头、图标或表格也可编辑时，选原生形状/图表/矢量方法，并明确交付范围，不能把位图底稿称为全矢量。

## 按需要读取

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
| 错字、漏标签、重复值、归属或布局错误 | [reconstruction repair](references/reconstruction-repair.md)、[text-region audit](references/text-region-audit.md)、[text reinspection](references/text-layout-reinspection.md) |
| 图标/连线上出现疑似 OCR 字符 | [OCR artifact sweep](references/ocr-artifact-sweep.md) |
| Octos peer 制作/返修：退出策略初始化、证据及生效核验 | [continuation evidence](references/continuation-evidence.md) |
| 需要可选结构检查或第二意见 | [host checks](references/host-checks.md) |

无需加载所有参考、为每个对象写长报告或重新编写测量框架。技能和脚本不得包含固定稿件的页码、文案、坐标或已知答案；页面数据可以包含当前任务实际确定的内容与坐标。

## 交付要求

- 以实际交付 PPTX 为检查对象。运行 `scripts/check_pptx_compat.py` 做限定结构检查；LibreOffice 成功不能证明 PowerPoint 能打开。测试过哪些应用就报告哪些。
- 查看最终实际渲染，核对文字与原文、数字/公式/单位、重复次数、字体、对齐、卡片归属、遮挡和小字。标题统一要求来自用户/风格约定。缺少 owner region 或整张大面板作为 owner，均不能证明局部标签摆放正确。
- 全稿与无字稿从同一母版派生并共享图形层。记录源图、裁图和最终输出尺寸；插值放大称“4K 输出”，不能称原生 4K 生图。上下成对图仍需检查几何与裁切。
- 给出 PPTX、预览、可复现源数据及用户要求的成对图片。说明可编辑范围、字体回退、已做检查与剩余问题。不要以哈希一致、文本框数量或机械检查通过替代美术验收。

复用宿主已有 peer 与退出能力，不另写裁决模型、轮询守护进程或第二套调度器。停滞窗口不是总轮数上限；时间/费用预算是另一项约束，只采用用户或宿主明确给定的值。技能约定、工具证据与内核强制能力分开报告。
