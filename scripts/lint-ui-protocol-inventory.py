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
    # Method-name literals never contain `//`, so line-level stripping is safe
    # here and keeps `///` doc comments between array elements harmless.
    return re.sub(r"^\s*//.*$", "", text, flags=re.M)


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
) -> list[str]:
    """Return human-readable violations; empty means the inventory is current."""
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
    return violations


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
    crate::ui_protocol::methods::SESSION_OPEN,
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

    # Clean sources: the lint passes with no violations.
    clean = check(SELF_TEST_CORE, SELF_TEST_TRANSPORT, SELF_TEST_INVENTORY, "inv.md")
    assert clean == [], clean

    # A method in code without an inventory row is caught.
    missing = check(
        SELF_TEST_CORE,
        SELF_TEST_TRANSPORT,
        SELF_TEST_INVENTORY.replace("| `turn/stop` |\n", ""),
        "inv.md",
    )
    assert any("`turn/stop`" in v and "missing" in v for v in missing), missing

    # A stale command row for a removed method is caught.
    stale = check(
        SELF_TEST_CORE,
        SELF_TEST_TRANSPORT,
        SELF_TEST_INVENTORY.replace(
            "| `turn/stop` |\n", "| `turn/stop` |\n| `turn/gone` |\n"
        ),
        "inv.md",
    )
    assert any("`turn/gone`" in v and "no longer" in v for v in stale), stale

    # Notification drift is caught on both sides.
    notif = check(
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
    escaped = check(escaped_core, SELF_TEST_TRANSPORT, SELF_TEST_INVENTORY, "inv.md")
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
        violations = check(core, transport, inventory)
    except ValueError as e:
        # The code shape outgrew the parser (or the inventory lost a table);
        # failing loudly beats passing vacuously.
        print(f"wire-inventory lint could not read its inputs: {e}", file=sys.stderr)
        return 1
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
