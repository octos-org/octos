#!/usr/bin/env python3
"""Keep the UI Protocol wire inventory honest against the code constants.

The inventory (`api/OCTOS_UI_PROTOCOL_WIRE_INVENTORY_2026-05-24.md`) claims to
reconcile the shipped wire surface with the spec and UPCR documents, but the
claim was never checked: `server/shutdown` (UPCR-2026-032) landed in the spec's
§6 method catalog on merge day while the inventory went unupdated, and three
more methods (`voice/admit`, `voice/commit_admission`,
`session/goal/operator_transition`) had drifted the same way by the time this
lint was written. The header's hand-written `Date:` went stale with them.

This lint replaces the trust with an executable check (the cross-boundary
convention: a reference gets a CI-runnable verification):

1. The Commands table must equal `UI_PROTOCOL_COMMAND_METHODS` union
   `APPUI_EXTRA_METHODS` — exactly the sources the inventory header declares.
2. The Notifications table must equal `UI_PROTOCOL_NOTIFICATION_METHODS`.
3. `UI_PROTOCOL_FIRST_SERVER_METHODS` must stay inside the command surface, so
   a refactor that changes the code shape fails loudly here instead of
   silently narrowing what the parser reads.

A method added in code without an inventory row — or a row left behind after
the method is gone — fails the `check` job.

Known blind spot: a wire notification emitted without appearing in
`UI_PROTOCOL_NOTIFICATION_METHODS` is invisible to those equality checks
(`turn/steer_dropped` and `session/orchestration` ship that way today; adding
them grows the `client_hello` `supported_notifications` negotiation surface,
so that is a code change, not a lint fix). Every wire-shaped `methods::`
constant outside the pinned surface is therefore reported as a non-blocking
warning, keeping the gap visible until the constants are completed.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

DEFAULT_INVENTORY = Path("api/OCTOS_UI_PROTOCOL_WIRE_INVENTORY_2026-05-24.md")
DEFAULT_CORE = Path("crates/octos-core/src/ui_protocol.rs")
DEFAULT_TRANSPORT = Path("crates/octos-cli/src/api/ui_protocol_transport.rs")


def _strip_line_comments(text: str) -> str:
    # Method-name literals never contain `//`, so anything from the first `//`
    # after a line's last quote is a comment. Whole-line comments (`///` doc
    # comments between array elements included) drop out entirely.
    stripped = []
    for line in text.splitlines():
        if line.lstrip().startswith("//"):
            continue
        marker = line.find("//")
        if marker != -1 and marker > line.rfind('"'):
            line = line[:marker]
        stripped.append(line)
    return "\n".join(stripped)


def parse_method_name_constants(core: str) -> dict[str, str]:
    """`pub mod methods` constants: `pub const X: &str = "x/y";`."""
    module = re.search(r"pub mod methods \{(.*?)\n\}", core, re.S)
    if not module:
        raise ValueError("could not find `pub mod methods` in octos-core ui_protocol.rs")
    names = {
        m.group(1): m.group(2)
        for m in re.finditer(r'pub const (\w+): &str = "([^"]*)";', module.group(1))
    }
    if not names:
        raise ValueError("parsed zero method-name constants from `pub mod methods`")
    return names


def _resolve_str_expr(expr: str, names: dict[str, str]) -> str | None:
    """A `&str` initializer: a literal or an alias to a `methods::` constant."""
    expr = expr.strip()
    literal = re.fullmatch(r'"([^"]*)"', expr)
    if literal:
        return literal.group(1)
    alias = re.search(r"methods::(\w+)\s*$", expr)
    if alias and alias.group(1) in names:
        return names[alias.group(1)]
    return None


def parse_const_aliases(text: str, names: dict[str, str]) -> dict[str, str]:
    """Module-local `const X: &str = ...;` bindings, multi-line aliases included."""
    aliases: dict[str, str] = {}
    for m in re.finditer(r"const (\w+):\s*&str\s*=\s*(.*?);", _strip_line_comments(text), re.S):
        resolved = _resolve_str_expr(m.group(2), names)
        if resolved is not None:
            aliases[m.group(1)] = resolved
    return aliases


def parse_str_list(text: str, list_name: str, names: dict[str, str]) -> list[str]:
    """Resolve a `const NAME: &[&str] = &[...]` array of method names.

    Every element must resolve — a literal, a `path::methods::X` reference, or
    a known `SCREAMING_CASE` alias — otherwise the list fails closed so the
    lint cannot silently under-read a new element shape.
    """
    m = re.search(
        r"(?:pub\s+)?const\s+" + list_name + r"\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
        _strip_line_comments(text),
        re.S,
    )
    if not m:
        raise ValueError(f"could not find const {list_name}")
    body = m.group(1)
    resolved: list[str] = []
    for element in body.split(","):
        element = element.strip()
        if not element:
            continue
        literal = re.fullmatch(r'"([^"]*)"', element)
        if literal:
            resolved.append(literal.group(1))
            continue
        method_ref = re.fullmatch(r"(?:\w+::)*methods::(\w+)", element)
        if method_ref and method_ref.group(1) in names:
            resolved.append(names[method_ref.group(1)])
            continue
        if element in names:
            resolved.append(names[element])
            continue
        raise ValueError(f"unrecognized element in {list_name}: {element!r}")
    if not resolved:
        raise ValueError(f"parsed zero elements from {list_name}")
    return resolved


def parse_inventory_tables(inventory: str) -> tuple[dict[str, int], dict[str, int]]:
    """Method -> first line number for the Commands and Notifications tables."""
    commands: dict[str, int] = {}
    notifications: dict[str, int] = {}
    section: str | None = None
    for line_no, line in enumerate(inventory.splitlines(), start=1):
        if line.startswith("## "):
            heading = line[3:].strip()
            section = "commands" if heading == "Commands" else (
                "notifications" if heading == "Notifications" else None
            )
            continue
        row = re.match(r"^\| `([^`]+)` \|", line)
        if not row or section is None:
            continue
        table = commands if section == "commands" else notifications
        if row.group(1) in table:
            raise ValueError(
                f"inventory line {line_no}: duplicate row `{row.group(1)}` in the "
                f"{section} table"
            )
        table[row.group(1)] = line_no
    if not commands or not notifications:
        raise ValueError("could not parse the Commands/Notifications tables in the inventory")
    return commands, notifications


def check(
    core: str,
    transport: str,
    inventory: str,
    inventory_name: str = str(DEFAULT_INVENTORY),
) -> tuple[list[str], list[str]]:
    """Return (violations, warnings); empty violations means the inventory is
    current with the pinned constants. Warnings cover wire-shaped constants
    the pinned lists don't name and never block."""
    names = parse_method_name_constants(core)
    names.update(parse_const_aliases(transport, names))
    commands = set(parse_str_list(core, "UI_PROTOCOL_COMMAND_METHODS", names))
    extra = parse_str_list(transport, "APPUI_EXTRA_METHODS", names)
    notifications = set(parse_str_list(core, "UI_PROTOCOL_NOTIFICATION_METHODS", names))
    first_server = set(parse_str_list(core, "UI_PROTOCOL_FIRST_SERVER_METHODS", names))
    inv_commands, inv_notifications = parse_inventory_tables(inventory)

    violations: list[str] = []
    if not first_server <= commands:
        violations.append(
            "UI_PROTOCOL_FIRST_SERVER_METHODS is no longer a subset of "
            f"UI_PROTOCOL_COMMAND_METHODS: {sorted(first_server - commands)} — update "
            "this lint's source-of-truth assumptions before trusting its output"
        )

    surface = commands | set(extra)
    for missing in sorted(surface - set(inv_commands)):
        violations.append(
            f"{inventory_name}: commands table is missing `{missing}` "
            "(shipped in code but not inventoried)"
        )
    for stale in sorted(set(inv_commands) - surface):
        violations.append(
            f"{inventory_name}:{inv_commands[stale]}: commands row `{stale}` "
            "is no longer in the code constants"
        )
    for missing in sorted(notifications - set(inv_notifications)):
        violations.append(
            f"{inventory_name}: notifications table is missing `{missing}` "
            "(shipped in code but not inventoried)"
        )
    for stale in sorted(set(inv_notifications) - notifications):
        violations.append(
            f"{inventory_name}:{inv_notifications[stale]}: notifications row "
            f"`{stale}` is no longer in the code constants"
        )

    # Wire methods are always `namespace/name`; the methods module also holds
    # feature gates and message fragments, which the shape excludes.
    method_shape = re.compile(r"^[a-z][a-z0-9_]*(?:/[a-z0-9_.]+)+$")
    warnings = [
        f"{inventory_name}: code constant `{name}` is in no pinned method list; "
        "if it ships on the wire, the inventory cannot see it — extend "
        "UI_PROTOCOL_NOTIFICATION_METHODS/UI_PROTOCOL_COMMAND_METHODS first"
        for name in sorted(
            {v for v in names.values() if method_shape.match(v)} - surface - notifications
        )
    ]
    return violations, warnings


# ---------------------------------------------------------------------------
# Self-test: synthetic sources exercising each parser branch and each check.
# ---------------------------------------------------------------------------

SELF_TEST_CORE = """
pub mod methods {
    pub const SESSION_OPEN: &str = "session/open";
    /// A documented method.
    pub const TURN_START: &str = "turn/start";
    pub const TURN_STOP: &str = "turn/stop";
    pub const GOAL_MOVE: &str = "session/goal/operator_transition";
    /// Emitted on the wire but absent from every pinned list.
    pub const ORPHAN_NOTE: &str = "turn/orphan_note";
    /// Not wire-shaped (a feature gate): never warned about.
    pub const ORPHAN_FEATURE: &str = "session.orphan_feature.v1";
}
pub const UI_PROTOCOL_COMMAND_METHODS: &[&str] = &[
    methods::SESSION_OPEN,
    methods::TURN_START,
    methods::GOAL_MOVE,
];
pub const UI_PROTOCOL_NOTIFICATION_METHODS: &[&str] = &[
    "turn/started",
];
pub const UI_PROTOCOL_FIRST_SERVER_METHODS: &[&str] = &[
    methods::SESSION_OPEN,
];
"""

SELF_TEST_TRANSPORT = """
const APPUI_METHOD_SHUTDOWN: &str =
    methods::TURN_STOP;
const APPUI_METHOD_PING: &str = "client_hello";
const APPUI_EXTRA_METHODS: &[&str] = &[
    APPUI_METHOD_PING,
    APPUI_METHOD_SHUTDOWN,
    // a comment between elements
    crate::ui_protocol::methods::SESSION_OPEN, // trailing element comment
];
"""

# Matches SELF_TEST_CORE ∪ SELF_TEST_TRANSPORT exactly, notifications included.
SELF_TEST_INVENTORY = (
    "## Commands\n\n| Method | Status |\n|---|---|\n"
    "| `session/open` |\n"
    "| `turn/start` |\n"
    "| `session/goal/operator_transition` |\n"
    "| `client_hello` |\n"
    "| `turn/stop` |\n"
    "\n## Notifications\n\n| Method | Status |\n|---|---|\n"
    "| `turn/started` |\n"
)


def _self_test() -> None:
    def expect_value_error(core: str, transport: str, inventory: str, needle: str) -> None:
        try:
            check(core, transport, inventory, "inv.md")
        except ValueError as e:
            assert needle in str(e), e
        else:
            raise AssertionError(f"expected ValueError containing {needle!r}")

    # Clean sources: no violations, and the pinned-list orphan is surfaced as
    # a warning while the non-wire-shaped feature constant stays silent.
    violations, warnings = check(
        SELF_TEST_CORE, SELF_TEST_TRANSPORT, SELF_TEST_INVENTORY, "inv.md"
    )
    assert violations == [], violations
    assert len(warnings) == 1 and "`turn/orphan_note`" in warnings[0], warnings

    # Trailing element comments parse instead of failing the array, and a
    # fully pinned surface produces zero warnings.
    covered_core = (SELF_TEST_CORE
        .replace('    /// Emitted on the wire but absent from every pinned list.\n'
                 '    pub const ORPHAN_NOTE: &str = "turn/orphan_note";\n', '')
        .replace('    /// Not wire-shaped (a feature gate): never warned about.\n'
                 '    pub const ORPHAN_FEATURE: &str = "session.orphan_feature.v1";\n', ''))
    violations, warnings = check(
        covered_core,
        SELF_TEST_TRANSPORT,  # carries `// trailing element comment`
        SELF_TEST_INVENTORY,
        "inv.md",
    )
    assert violations == [] and warnings == [], (violations, warnings)

    # A method in code without an inventory row is caught.
    missing, _ = check(
        SELF_TEST_CORE,
        SELF_TEST_TRANSPORT,
        SELF_TEST_INVENTORY.replace("| `turn/stop` |\n", ""),
        "inv.md",
    )
    assert any("`turn/stop`" in v and "missing" in v for v in missing), missing

    # A stale command row for a removed method is caught.
    stale, _ = check(
        SELF_TEST_CORE,
        SELF_TEST_TRANSPORT,
        SELF_TEST_INVENTORY.replace(
            "| `turn/stop` |\n", "| `turn/stop` |\n| `turn/gone` |\n"
        ),
        "inv.md",
    )
    assert any("`turn/gone`" in v and "no longer" in v for v in stale), stale

    # Notification drift is caught on both sides.
    notif, _ = check(
        SELF_TEST_CORE,
        SELF_TEST_TRANSPORT,
        SELF_TEST_INVENTORY.replace("| `turn/started` |", "| `turn/wrong` |"),
        "inv.md",
    )
    assert any("notifications table is missing `turn/started`" in v for v in notif), notif
    assert any("`turn/wrong`" in v and "no longer" in v for v in notif), notif

    # A duplicate row is caught while parsing.
    expect_value_error(
        SELF_TEST_CORE,
        SELF_TEST_TRANSPORT,
        SELF_TEST_INVENTORY.replace(
            "| `turn/stop` |\n", "| `turn/stop` |\n| `turn/stop` |\n"
        ),
        "duplicate row",
    )

    # FIRST_SERVER_METHODS escaping the command surface fails loudly.
    escaped_core = SELF_TEST_CORE.replace(
        "pub const UI_PROTOCOL_COMMAND_METHODS: &[&str] = &[\n    methods::SESSION_OPEN,",
        "pub const UI_PROTOCOL_COMMAND_METHODS: &[&str] = &[",
    )
    escaped, _ = check(escaped_core, SELF_TEST_TRANSPORT, SELF_TEST_INVENTORY, "inv.md")
    assert any("no longer a subset" in v for v in escaped), escaped

    # An unrecognized element shape fails closed instead of under-reading.
    expect_value_error(
        SELF_TEST_CORE,
        SELF_TEST_TRANSPORT.replace("    APPUI_METHOD_PING,", "    some_new_form(),"),
        SELF_TEST_INVENTORY,
        "unrecognized element",
    )

    print("self-test ok")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true", help="run synthetic fixtures only")
    args = parser.parse_args()
    if args.self_test:
        _self_test()
        return 0

    inventory = DEFAULT_INVENTORY.read_text()
    core = DEFAULT_CORE.read_text()
    transport = DEFAULT_TRANSPORT.read_text()
    try:
        violations, warnings = check(core, transport, inventory)
    except ValueError as e:
        # The code shape outgrew the parser (or the inventory lost a table);
        # failing loudly beats passing vacuously.
        print(f"wire-inventory lint could not read its inputs: {e}", file=sys.stderr)
        return 1
    for warning in warnings:
        print(f"warning: {warning}", file=sys.stderr)
    if violations:
        for violation in violations:
            print(violation, file=sys.stderr)
        print(
            f"{len(violations)} wire-inventory violation(s); update "
            f"{DEFAULT_INVENTORY} alongside the code constants",
            file=sys.stderr,
        )
        return 1
    print(f"wire inventory current with the code constants ({DEFAULT_INVENTORY})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
