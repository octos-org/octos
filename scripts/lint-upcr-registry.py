#!/usr/bin/env python3
"""Keep the UPCR file namespace unambiguous.

`docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_NNN[_SLUG].md` filenames are
the protocol's decision-record foreign keys: the spec, the wire inventory, and
Rust doc comments cite records as `UPCR-2026-NNN`. Nothing checked that the
NNN was unique or that filenames followed one shape, so three numbers (022,
026, 027) ended up claimed by two unrelated files each, and the registry
introduced alongside this lint (#2423) had no mechanical backstop. A duplicate
silently reopens that ambiguity — the exact failure that forced #2730 to
renumber a freshly-landed record.

This lint fails the `check` job when:

1. Two files claim the same `UPCR-2026-NNN` number.
2. A filename deviates from
   `OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_NNN_SHORT_SLUG.md` (uppercase
   underscore slug; the bare `..._2026_022.md` form was the collision's
   other symptom).

Registering a newly claimed number in `docs/upcr/README.md` is a review
convention, not a lint rule — the lint only enforces what can be checked
mechanically without a second source of truth.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

UPCR_DIR = Path("docs")
UPCR_PATTERN = re.compile(
    r"^OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_(\d{3})(?:_([A-Z0-9_]+))?\.md$"
)


def check_upcr_filenames(names: list[str]) -> list[str]:
    """Check a list of UPCR *basenames* for number collisions and shape."""
    problems: list[str] = []
    seen: dict[str, str] = {}
    for name in sorted(names):
        match = UPCR_PATTERN.match(name)
        if match is None:
            problems.append(f"non-conforming UPCR filename (want ..._2026_NNN_SHORT_SLUG.md): {name}")
            continue
        number = f"UPCR-2026-{match.group(1)}"
        if match.group(2) is None:
            problems.append(f"{name}: missing SHORT_SLUG suffix (bare-number filename)")
        previous = seen.get(number)
        if previous is not None:
            problems.append(f"duplicate {number}: {previous} and {name}")
        else:
            seen[number] = name
    return problems


def run_checks(repo: Path) -> list[str]:
    names = [
        path.name
        for path in sorted((repo / UPCR_DIR).glob("OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_*"))
        if path.is_file()
    ]
    return check_upcr_filenames(names)


def self_test() -> int:
    failures: list[str] = []

    def expect_problems(label: str, problems: list[str], needle: str) -> None:
        if not any(needle in p for p in problems):
            failures.append(f"{label}: expected a problem containing {needle!r}, got {problems}")

    good = [
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_001_TYPED_APPROVAL.md",
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_WIRE_CONTRACTS_BACKFILL.md",
        "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_043_SESSION_SANDBOX_NARROWING.md",
    ]
    if check_upcr_filenames(good):
        failures.append(f"clean set flagged: {check_upcr_filenames(good)}")

    duplicate = good + ["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_SESSION_POLICY.md"]
    expect_problems("duplicate number", check_upcr_filenames(duplicate), "duplicate UPCR-2026-022")

    expect_problems(
        "bare filename",
        check_upcr_filenames(["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md"]),
        "missing SHORT_SLUG",
    )
    # A bare-number filename must still register its number for the duplicate
    # check — a `continue` after the slug warning would silently let a bare
    # file and a slugged file share a number again.
    bare_collision = check_upcr_filenames(
        [
            "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022.md",
            "OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_WIRE_CONTRACTS_BACKFILL.md",
        ]
    )
    expect_problems("bare/slug collision", bare_collision, "duplicate UPCR-2026-022")
    expect_problems(
        "bad slug",
        check_upcr_filenames(["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_022_wire-contracts.md"]),
        "non-conforming UPCR filename",
    )
    expect_problems(
        "missing number",
        check_upcr_filenames(["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_NO_NUMBER.md"]),
        "non-conforming UPCR filename",
    )
    # Exactly three digits: a four-digit number must not parse as NNN + junk.
    expect_problems(
        "four-digit number",
        check_upcr_filenames(["OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_0123_RECORD.md"]),
        "non-conforming UPCR filename",
    )

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("ok: self-test")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run the checker against synthetic fixtures instead of the repo",
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()

    repo = Path(__file__).resolve().parent.parent
    problems = run_checks(repo)
    if problems:
        for p in problems:
            print(f"FAIL: {p}")
        if any("duplicate" in p for p in problems):
            print(
                "Claim the next free number (check docs/upcr/README.md and the "
                "in-flight note) and add your row in the same PR."
            )
        else:
            print("Rename to OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_NNN_SHORT_SLUG.md.")
        return 1
    print("ok: UPCR filename namespace (#2423)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
