#!/usr/bin/env python3
"""Generate README SVGs with `python3 scripts/render_readme_diagrams.py`.

The illustrations are self-contained and require no third-party dependencies.
Edit this source and regenerate all four assets together.
"""

from html import escape
from pathlib import Path


OUTPUT = Path(__file__).resolve().parents[1] / "docs" / "assets" / "readme"
INK = "#182b45"
MUTED = "#5c6f86"
BLUE = "#3468ce"
TEAL = "#168575"
LINE = "#b5c6d8"


class Canvas:
    def __init__(self, height, title, description):
        self.parts = [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="{height}" '
            f'viewBox="0 0 1200 {height}" role="img" aria-labelledby="title description">',
            f"<title id=\"title\">{escape(title)}</title>",
            f"<desc id=\"description\">{escape(description)}</desc>",
            '<defs><linearGradient id="paper" x2="1" y2="1">'
            '<stop stop-color="#f5f9ff"/><stop offset="1" stop-color="#f7fbf9"/>'
            '</linearGradient><linearGradient id="kernel" x2="1" y2="1">'
            '<stop stop-color="#173f73"/><stop offset="1" stop-color="#1e3156"/>'
            '</linearGradient><marker id="arrow" viewBox="0 0 10 10" refX="8" refY="5" '
            'markerWidth="6" markerHeight="6" orient="auto-start-reverse">'
            '<path d="M 1 1 L 9 5 L 1 9" fill="none" stroke="context-stroke" '
            'stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>'
            '</marker></defs>',
            '<style>text{font-family:Inter,-apple-system,BlinkMacSystemFont,"Segoe UI",'
            '"PingFang SC","Microsoft YaHei",sans-serif} '
            '.code{font-family:ui-monospace,SFMono-Regular,Consolas,monospace}</style>',
        ]
        self.rect(0, 0, 1200, height, "url(#paper)", radius=28)

    def rect(self, x, y, width, height, fill, stroke="none", radius=16):
        self.parts.append(
            f'<rect x="{x}" y="{y}" width="{width}" height="{height}" '
            f'rx="{radius}" fill="{fill}" stroke="{stroke}"/>'
        )

    def text(self, x, y, value, size=18, color=INK, weight=400, anchor="start", code=False):
        attributes = ' class="code"' if code else ""
        self.parts.append(
            f'<text x="{x}" y="{y}" font-size="{size}" font-weight="{weight}" '
            f'fill="{color}" text-anchor="{anchor}"'
            f'{attributes}>{escape(value)}</text>'
        )

    def line(self, path, color=LINE, arrow=True, reverse=False, dashed=False):
        attributes = ' marker-end="url(#arrow)"' if arrow else ""
        if reverse:
            attributes += ' marker-start="url(#arrow)"'
        if dashed:
            attributes += ' stroke-dasharray="5 7"'
        self.parts.append(
            f'<path d="{path}" fill="none" stroke="{color}" stroke-width="2" '
            'stroke-linecap="round" stroke-linejoin="round"'
            f'{attributes}/>'
        )

    def icon(self, kind, x, y, color=BLUE, scale=1):
        paths = {
            "app": '<rect x="2" y="3" width="24" height="21" rx="4"/><path d="M2 10h24"/><path d="M7 6.5h.1M11 6.5h.1"/>',
            "code": '<path d="m9 6-7 8 7 8m10-16 7 8-7 8M16 3l-4 22"/>',
            "agents": '<circle cx="14" cy="6" r="4"/><circle cx="5" cy="20" r="4"/><circle cx="23" cy="20" r="4"/><path d="M11 10 7 16m10-6 4 6M9 20h10"/>',
            "exchange": '<path d="M3 9h22m-6-6 6 6-6 6M25 21H3m6-6-6 6 6 6"/>',
            "memory": '<path d="m2 8 12-6 12 6-12 6-12-6Zm0 7 12 6 12-6M2 22l12 6 12-6"/>',
            "workflow": '<rect x="1" y="1" width="9" height="9" rx="2"/><rect x="18" y="18" width="9" height="9" rx="2"/><path d="M10 5h12v13M5 10v12h13"/>',
            "hub": '<circle cx="14" cy="14" r="5"/><circle cx="14" cy="2" r="1.5"/><circle cx="26" cy="14" r="1.5"/><circle cx="14" cy="26" r="1.5"/><circle cx="2" cy="14" r="1.5"/><path d="M14 4v5m5 5h5M14 19v5M4 14h5M6 6l4 4m8 8 4 4M6 22l4-4m8-8 4-4"/>',
        }
        self.parts.append(
            f'<g transform="translate({x} {y}) scale({scale})" fill="none" '
            f'stroke="{color}" stroke-width="1.8" stroke-linecap="round" '
            f'stroke-linejoin="round">{paths[kind]}</g>'
        )

    def write(self, filename):
        OUTPUT.mkdir(parents=True, exist_ok=True)
        (OUTPUT / filename).write_text(
            "\n".join(self.parts + ["</svg>"]) + "\n", encoding="utf-8"
        )


def architecture(zh=False):
    choose = lambda en, cn: cn if zh else en
    c = Canvas(
        790,
        choose("Octos kernel architecture", "Octos 内核架构"),
        choose(
            "Applications and agent controllers use OUP; native hosts embed libraries. Both reach the Octos harness kernel and its state, execution, and coordination capabilities.",
            "应用和 Agent 控制端通过 OUP 接入；原生宿主嵌入库。两条路径连接 Octos Harness 内核，使用状态、执行与协作能力。",
        ),
    )
    c.text(48, 53, choose("OCTOS / ARCHITECTURE", "OCTOS / 内核架构"), 14, BLUE, 650)
    c.text(48, 101, choose("Your application, powered by Octos", "用 Octos 内核构建你的应用"), 34, weight=650)
    c.text(48, 135, choose("Embed the libraries or control a runtime through OUP.", "将库嵌入应用，或通过 OUP 控制运行时。"), 19, MUTED)

    entries = [
        (48, "app", choose("Applications", "应用客户端"), "Octoscode · Octoscode Web"),
        (424, "agents", choose("Agent controllers", "Agent 控制端"), "Codex · Claude Code"),
        (800, "code", choose("Your native app", "你的原生应用"), choose("Desktop · service · device", "桌面应用 · 服务 · 设备")),
    ]
    for x, icon, title, subtitle in entries:
        c.rect(x, 174, 352, 96, "#ffffff", "#dce5ef")
        c.icon(icon, x + 24, 199)
        c.text(x + 68, 211, title, 22, weight=600)
        c.text(x + 24, 244, subtitle, 18, MUTED)

    c.line("M224 276V312", TEAL, reverse=True)
    c.line("M600 276V312", TEAL, reverse=True)
    c.rect(526, 282, 148, 23, "#f6fafc", radius=5)
    c.text(600, 298, choose("Your OUP adapter", "你的 OUP 适配器"), 14, TEAL, anchor="middle")
    c.line("M976 276V312", BLUE, reverse=True)

    c.rect(48, 318, 728, 76, "#eaf7f2", "#c4e5d9")
    c.icon("exchange", 72, 341, TEAL)
    c.text(116, 365, "OUP", 26, TEAL, 700)
    c.text(197, 364, choose("Commands · state · events", "命令 · 状态 · 事件"), 19, TEAL)
    c.text(752, 364, "WebSocket / stdio", 16, TEAL, anchor="end")
    c.rect(800, 318, 352, 76, "#eef3fd", "#d2def4")
    c.text(824, 348, choose("Library API", "库接口"), 22, BLUE, 650)
    c.text(824, 376, choose("Rust crates · task bindings", "Rust crates · 任务执行绑定"), 18, MUTED)

    c.line("M412 400V434", TEAL, reverse=True)
    c.line("M976 400V434", BLUE, reverse=True)
    c.rect(48, 440, 1104, 100, "url(#kernel)", radius=20)
    c.rect(68, 461, 58, 58, "#2b5280", radius=15)
    c.icon("hub", 82, 475, "#a1eddd", 1.1)
    c.text(148, 482, choose("Octos harness kernel", "Octos Harness 内核"), 31, "#ffffff", 650)
    c.text(148, 516, choose("Agent execution · sessions · supervision", "Agent 执行 · 会话 · 监督"), 19, "#cadbf1")
    c.rect(920, 471, 200, 38, "#29496f", "#4a688c", 19)
    c.text(1020, 496, choose("Written in Rust", "使用 Rust 编写"), 18, "#e2f1ff", 500, "middle")

    features = [
        (48, "memory", choose("Context & memory", "上下文与记忆"), choose("Compaction · retrieval", "压缩 · 检索"), choose("Durable history · replay", "持久化历史 · 回放")),
        (424, "workflow", choose("Tools & workflows", "工具与工作流"), choose("Skills · model routing", "技能 · 模型路由"), choose("Permissions · sandboxing", "权限 · 沙箱")),
        (800, "agents", choose("Agents & peers", "Agent 与 Peer"), choose("Delegation · worktrees", "任务委派 · worktree"), choose("Tasks · artifacts · results", "任务 · 产物 · 结果")),
    ]
    for x, icon, title, first, second in features:
        c.line(f"M{x + 176} 546V580")
        c.rect(x, 586, 352, 134, "#ffffff", "#dce5ef")
        c.icon(icon, x + 24, 607, TEAL)
        c.text(x + 68, 629, title, 22, weight=600)
        c.text(x + 24, 666, first, 19, MUTED)
        c.text(x + 24, 695, second, 19, MUTED)
    c.text(48, 762, choose("Shared execution and state for your clients and controllers.", "为应用客户端与 Agent 控制端提供统一的执行与状态。"), 18, MUTED)
    c.write("architecture-zh.svg" if zh else "architecture.svg")


def workflow(zh=False):
    choose = lambda en, cn: cn if zh else en
    c = Canvas(
        920,
        choose("An OUP-controlled workflow", "通过 OUP 控制的工作流"),
        choose(
            "Connect, assign, supervise, and collect. The controller exchanges commands and events with the kernel while it prepares context, calls models, and executes tools as needed. Acceptance precedes the completed, failed, or interrupted outcome.",
            "连接、分配、监督、收集。控制端与内核交换命令和事件，内核按需准备上下文、调用模型和执行工具。接纳请求后，轮次最终以完成、失败或中断结束。",
        ),
    )
    c.text(48, 53, choose("OCTOS / WORKFLOW", "OCTOS / 执行流程"), 14, BLUE, 650)
    c.text(48, 101, choose("From a request to a recorded result", "从任务请求到可追踪的结果"), 34, weight=650)
    c.text(48, 135, choose("Your controller directs the work. The kernel runs it.", "控制端组织工作，内核负责执行。"), 19, MUTED)
    stages = [
        ("01", choose("Connect", "连接"), choose("Negotiate · open a session", "协商能力 · 打开会话"), "session/open"),
        ("02", choose("Assign", "分配"), choose("Start a scoped turn", "启动有明确作用域的轮次"), "turn/start"),
        ("03", choose("Supervise", "监督"), choose("Observe · respond · steer", "观察 · 回应 · 引导"), "tool/* · turn/steer"),
        ("04", choose("Collect", "收集"), choose("Inspect final outcomes", "检查最终结果"), "turn/completed · turn/error"),
    ]
    for i, (number, title, subtitle, method) in enumerate(stages):
        x = 48 + i * 284
        c.rect(x, 174, 252, 142, "#ffffff", "#dce5ef")
        c.rect(x + 18, 193, 34, 28, "#edf3ff", radius=8)
        c.text(x + 35, 213, number, 15, BLUE, 650, "middle")
        c.text(x + 64, 215, title, 23, weight=650)
        c.text(x + 18, 252, subtitle, 17, MUTED)
        c.text(x + 18, 291, method, 13, BLUE, code=True)
        if i < 3:
            c.line(f"M{x + 258} 241h19")

    c.rect(48, 356, 1104, 428, "#ffffff", "#dce5ef", 22)
    c.text(80, 392, choose("DURING THE TURN", "轮次执行期间"), 14, MUTED, 650)
    c.rect(80, 416, 270, 288, "#eef3fd", "#d2def4")
    c.icon("app", 103, 440)
    c.text(145, 461, choose("Your controller", "你的控制端"), 23, BLUE, 650)
    c.text(103, 491, choose("Owns coordination policy", "负责协调策略"), 17, MUTED)
    actions = [
        choose("Assign a task", "分配任务"),
        choose("Inspect progress", "检查进展"),
        choose("Resolve approvals", "回应审批与问题"),
        choose("Steer or interrupt", "引导或中断"),
    ]
    for i, action in enumerate(actions):
        y = 535 + i * 42
        c.rect(104, y - 13, 6, 6, "#6d96de", radius=3)
        c.text(124, y, action, 19, INK)

    c.line("M356 520H464", BLUE)
    c.text(410, 499, choose("Commands", "命令"), 15, BLUE, 500, "middle")
    c.line("M464 651H356", TEAL, dashed=True)
    c.text(410, 633, choose("Events", "事件"), 15, TEAL, 500, "middle")
    c.rect(470, 416, 650, 288, "url(#kernel)", radius=18)
    c.text(496, 461, choose("Octos kernel", "Octos 内核"), 25, "#ffffff", 650)
    c.rect(939, 436, 153, 31, "#2b5280", radius=15)
    c.text(1015, 457, choose("Turn running", "轮次执行中"), 16, "#b9f2e1", 500, "middle")
    steps = [
        (496, choose("Context", "上下文"), choose("Prepare / compact", "准备 / 压缩")),
        (704, choose("Model", "模型"), choose("Decide next step", "决定下一步")),
        (912, choose("Tools", "工具"), choose("When requested", "按需执行")),
    ]
    for i, (x, title, subtitle) in enumerate(steps):
        c.rect(x, 507, 180, 91, "#2b4c77", "#49678e", 12)
        c.text(x + 90, 543, title, 22, "#ffffff", 600, "middle")
        c.text(x + 90, 576, subtitle, 16, "#c6d8ef", anchor="middle")
        if i < 2:
            c.line(f"M{x + 186} 552h16", "#8dcfc6")
    c.line("M1002 606V646H586V606", "#8dcfc6")
    c.rect(685, 631, 220, 29, "#1e365d", radius=8)
    c.text(795, 651, choose("Continue as needed", "按需继续下一步"), 16, "#cae9e5", anchor="middle")
    c.text(80, 746, choose("A pending approval or question waits for the controller's response through OUP.", "等待中的审批或问题，通过 OUP 接收控制端的回应。"), 18, MUTED)

    c.text(48, 833, choose("FINAL OUTCOME", "最终结果"), 14, MUTED, 650)
    for x, title, fill, color in [
        (244, choose("Completed", "完成"), "#e6f5ed", "#297353"),
        (526, choose("Failed", "失败"), "#faedee", "#a44a59"),
        (808, choose("Interrupted", "中断"), "#f8f0df", "#916f25"),
    ]:
        c.rect(x, 805, 256, 44, fill, radius=22)
        c.text(x + 128, 834, title, 19, color, 600, "middle")
    c.text(48, 889, choose("Acceptance starts the work; a terminal event closes the turn.", "请求被接纳后开始执行，终结事件标志着本轮结束。"), 18, MUTED)
    c.write("workflow-zh.svg" if zh else "workflow.svg")


if __name__ == "__main__":
    for chinese in (False, True):
        architecture(chinese)
        workflow(chinese)
    print(f"Wrote four SVG illustrations to {OUTPUT}")
